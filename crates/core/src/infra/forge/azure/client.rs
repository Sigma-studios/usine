//! The HTTP side of the Azure DevOps forge: credentials, requests, errors.
//!
//! No token is stored by the app — the same stance as the GitHub forge's
//! reuse of `gh auth`. A request authenticates with, in order:
//! 1. a personal access token in `AZURE_DEVOPS_EXT_PAT` (the variable the
//!    Azure CLI's own DevOps extension reads), sent as Basic auth;
//! 2. an Entra token from the signed-in Azure CLI
//!    (`az account get-access-token`), cached in memory until shortly before
//!    it expires and refreshed on a 401.

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::wire;
use crate::error::{CoreError, Result};

/// The environment variable holding a personal access token.
pub const PAT_ENV: &str = "AZURE_DEVOPS_EXT_PAT";

/// Entra's resource id for Azure DevOps — what an `az` token must be minted for.
const AZURE_DEVOPS_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";

/// Cap on any single request, so a stalled connection can't hang a run actor.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Cap on the `az` token command (it may refresh a token over the network).
const AZ_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Http {
    client: reqwest::Client,
    /// A token fixed at construction, bypassing the lookup (tests).
    pat: Option<String>,
    /// The cached `az` token and when to stop trusting it.
    bearer: Mutex<Option<(String, Instant)>>,
}

impl Http {
    /// `pat` pins the credential; `None` looks it up per request (see the
    /// module docs).
    pub fn new(pat: Option<String>) -> Self {
        Http {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent(concat!("usine/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            pat,
            bearer: Mutex::new(None),
        }
    }

    /// The `Authorization` header value for the next request.
    async fn authorization(&self) -> Result<String> {
        let pat = self
            .pat
            .clone()
            .or_else(|| std::env::var(PAT_ENV).ok())
            .filter(|p| !p.trim().is_empty());
        if let Some(pat) = pat {
            return Ok(format!(
                "Basic {}",
                base64(format!(":{}", pat.trim()).as_bytes())
            ));
        }
        let mut cached = self.bearer.lock().await;
        if let Some((token, valid_until)) = cached.as_ref() {
            if Instant::now() < *valid_until {
                return Ok(format!("Bearer {token}"));
            }
        }
        let (token, valid_for) = az_token().await?;
        *cached = Some((token.clone(), Instant::now() + valid_for));
        Ok(format!("Bearer {token}"))
    }

    async fn forget_bearer(&self) {
        *self.bearer.lock().await = None;
    }

    /// Send a JSON request and parse the JSON response (`Null` for an empty
    /// body). See [`Self::send`] for the retries and error shape.
    pub async fn json(&self, method: Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let text = self.send(method, url, body, "application/json").await?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// GET a plain-text resource (a build log).
    pub async fn text(&self, url: &str) -> Result<String> {
        self.send(Method::GET, url, None, "text/plain").await
    }

    /// One request, with the two retries Azure DevOps makes necessary:
    /// - a 401 on a cached `az` token drops the token and tries a fresh one;
    /// - an `api-version=7.1` refused because the resource is still in preview
    ///   in that version is retried as `7.1-preview`, so each call can ask for
    ///   the GA version without tracking which resources haven't graduated.
    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
        accept: &str,
    ) -> Result<String> {
        let mut url = url.to_string();
        let mut retried_auth = false;
        let mut retried_preview = false;
        loop {
            let auth = self.authorization().await?;
            let mut req = self
                .client
                .request(method.clone(), &url)
                .header("Authorization", auth.as_str())
                .header("Accept", accept);
            if let Some(body) = body {
                let content_type = if method == Method::PATCH && body.is_array() {
                    "application/json-patch+json"
                } else {
                    "application/json"
                };
                req = req
                    .header("Content-Type", content_type)
                    .body(serde_json::to_vec(body)?);
            }
            let resp = req.send().await.map_err(|e| {
                CoreError::forge(format!(
                    "Azure DevOps {method} {} failed: {e}",
                    path_of(&url)
                ))
            })?;
            let status = resp.status();
            let is_html = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/html"));
            let text = resp.text().await.unwrap_or_default();

            // A bad PAT doesn't get a 401: Azure answers 203 with its HTML
            // sign-in page.
            if status == StatusCode::UNAUTHORIZED || (status.is_success() && is_html) {
                if !retried_auth && !auth.starts_with("Basic") {
                    retried_auth = true;
                    self.forget_bearer().await;
                    continue;
                }
                return Err(CoreError::forge(auth_failure(&auth)));
            }
            if status.is_success() {
                return Ok(text);
            }
            let message = wire::error_message(&text);
            if status == StatusCode::BAD_REQUEST
                && !retried_preview
                && message.contains("-preview")
                && url.contains("api-version=7.1")
                && !url.contains("api-version=7.1-preview")
            {
                retried_preview = true;
                url = url.replace("api-version=7.1", "api-version=7.1-preview");
                continue;
            }
            return Err(CoreError::forge(format!(
                "Azure DevOps {method} {} failed (HTTP {}): {message}",
                path_of(&url),
                status.as_u16()
            )));
        }
    }
}

fn auth_failure(auth: &str) -> String {
    if auth.starts_with("Basic") {
        format!(
            "Azure DevOps refused the personal access token in {PAT_ENV} — check it hasn't \
             expired and has the Code (read & write) scope for this organization"
        )
    } else {
        "Azure DevOps refused the Azure CLI's token — run `az login` again (with an account \
         that belongs to this organization), or set a personal access token in \
         AZURE_DEVOPS_EXT_PAT"
            .to_string()
    }
}

/// The URL minus scheme, host and query — what an error message names.
fn path_of(url: &str) -> &str {
    let no_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = no_scheme.find('/').map(|i| &no_scheme[i..]).unwrap_or("/");
    path.split('?').next().unwrap_or(path)
}

/// An Entra access token for Azure DevOps from the signed-in Azure CLI, and
/// how long to trust it (until five minutes before it expires).
async fn az_token() -> Result<(String, Duration)> {
    let out = tokio::time::timeout(
        AZ_TIMEOUT,
        Command::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                AZURE_DEVOPS_RESOURCE,
                "--output",
                "json",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| CoreError::forge("`az account get-access-token` timed out"))?
    .map_err(|_| no_credentials())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(CoreError::forge(format!(
            "{} (`az account get-access-token` failed: {})",
            no_credentials(),
            stderr.trim()
        )));
    }
    parse_az_token(&String::from_utf8_lossy(&out.stdout), now_unix())
        .ok_or_else(|| CoreError::forge("unexpected output from `az account get-access-token`"))
}

fn no_credentials() -> CoreError {
    CoreError::forge(format!(
        "no Azure DevOps credentials: set {PAT_ENV} to a personal access token, or install \
         the Azure CLI and run `az login`"
    ))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read `az account get-access-token --output json`: the token, and how long
/// to use it — until five minutes before `expires_on` (a Unix timestamp in
/// current CLIs; older ones only print a local `expiresOn`, so assume the
/// standard hour).
fn parse_az_token(json: &str, now: u64) -> Option<(String, Duration)> {
    let v: Value = serde_json::from_str(json).ok()?;
    let token = v.get("accessToken")?.as_str()?.to_string();
    let expires_on = v.get("expires_on").and_then(|e| {
        e.as_u64()
            .or_else(|| e.as_str().and_then(|s| s.parse().ok()))
    });
    let lifetime = match expires_on {
        Some(at) => at.saturating_sub(now),
        None => 3600,
    };
    Some((token, Duration::from_secs(lifetime.saturating_sub(300))))
}

/// Standard base64 (for the Basic auth header).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            (":pat", "OnBhdA=="),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
    }

    #[test]
    fn az_tokens_are_trusted_until_five_minutes_before_expiry() {
        let json = r#"{"accessToken":"tok","expires_on":10000,"expiresOn":"2026-01-01 00:00:00"}"#;
        assert_eq!(
            parse_az_token(json, 10000 - 3600),
            Some(("tok".to_string(), Duration::from_secs(3300)))
        );
        // A string timestamp, and none at all (older CLIs).
        let (_, d) = parse_az_token(r#"{"accessToken":"t","expires_on":"10000"}"#, 9000).unwrap();
        assert_eq!(d, Duration::from_secs(700));
        let (_, d) = parse_az_token(r#"{"accessToken":"t"}"#, 0).unwrap();
        assert_eq!(d, Duration::from_secs(3300));
        assert_eq!(parse_az_token("nope", 0), None);
    }

    #[test]
    fn errors_name_the_path_without_host_or_query() {
        assert_eq!(
            path_of(
                "https://dev.azure.com/o/p/_apis/git/repositories/r/pullrequests?api-version=7.1"
            ),
            "/o/p/_apis/git/repositories/r/pullrequests"
        );
        assert_eq!(path_of("https://dev.azure.com"), "/");
    }
}
