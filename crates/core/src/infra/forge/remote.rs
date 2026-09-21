//! Recognizing a project's code host from its `origin` URL.
//!
//! Pure string work, unit-tested against every shape the hosts document: the
//! result seeds [`crate::ProjectConfig::detected_forge`] at startup and gives
//! the Azure DevOps client the organization/project/repository it addresses
//! (GitHub needs nothing — `gh` infers owner/repo from the checkout itself).

/// What an `origin` URL points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteForge {
    GitHub,
    AzureDevOps(AzureRepo),
}

/// An Azure Repos repository, addressed the way its REST API wants it. Names
/// are stored *decoded* (a project called `My Project` is `My Project` here,
/// `My%20Project` in the URL) and re-encoded by the URL builders.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AzureRepo {
    pub org: String,
    pub project: String,
    pub repo: String,
}

impl AzureRepo {
    /// `https://dev.azure.com/{org}` — the organization root, which also
    /// serves orgs still reached through a legacy `*.visualstudio.com` remote.
    pub fn org_url(&self) -> String {
        format!("https://dev.azure.com/{}", encode_segment(&self.org))
    }

    /// `https://dev.azure.com/{org}/{project}`.
    pub fn project_url(&self) -> String {
        format!("{}/{}", self.org_url(), encode_segment(&self.project))
    }

    /// The Git REST root for this repository:
    /// `…/{project}/_apis/git/repositories/{repo}`.
    pub fn git_api(&self) -> String {
        format!(
            "{}/_apis/git/repositories/{}",
            self.project_url(),
            encode_segment(&self.repo)
        )
    }

    /// The repository's web page, `…/{project}/_git/{repo}`.
    pub fn web_url(&self) -> String {
        format!("{}/_git/{}", self.project_url(), encode_segment(&self.repo))
    }

    /// A pull request's web page (what the "Open on Azure DevOps" links use;
    /// the REST payloads' own `url` is an API address).
    pub fn pr_web_url(&self, pr_id: u64) -> String {
        format!("{}/pullrequest/{pr_id}", self.web_url())
    }
}

/// Classify a git remote URL. `None` for anything not recognizably GitHub or
/// Azure DevOps Services (a GitHub Enterprise host, an SSH alias, a self-
/// hosted server) — the project then keeps its pinned or default forge.
///
/// Accepted Azure DevOps shapes:
/// - `https://[user@]dev.azure.com/{org}/{project}/_git/{repo}`, and the
///   short `…/{org}/_git/{repo}` used when the repo is named after its project;
/// - `https://{org}.visualstudio.com[/DefaultCollection]/{project}/_git/{repo}`;
/// - `git@ssh.dev.azure.com:v3/{org}/{project}/{repo}` and its
///   `ssh://git@ssh.dev.azure.com[:22]/v3/…` spelling;
/// - `{org}@vs-ssh.visualstudio.com:v3/{org}/{project}/{repo}`.
pub fn parse_remote(url: &str) -> Option<RemoteForge> {
    let (host, path) = split_host_path(url)?;
    let segments = path_segments(&path);
    let seg: Vec<&str> = segments.iter().map(String::as_str).collect();

    if host == "github.com" || host.ends_with(".github.com") {
        return Some(RemoteForge::GitHub);
    }
    if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" {
        // `v3/{org}/{project}/{repo}`.
        return match seg.as_slice() {
            [v, org, project, repo] if v.eq_ignore_ascii_case("v3") => azure(org, project, repo),
            _ => None,
        };
    }
    if host == "dev.azure.com" {
        return match seg.as_slice() {
            [org, project, git, repo] if *git == "_git" => azure(org, project, repo),
            [org, git, repo] if *git == "_git" => azure(org, repo, repo),
            _ => None,
        };
    }
    if let Some(org) = host.strip_suffix(".visualstudio.com") {
        let rest: &[&str] = match seg.as_slice() {
            [first, rest @ ..] if first.eq_ignore_ascii_case("DefaultCollection") => rest,
            all => all,
        };
        return match rest {
            [project, git, repo] if *git == "_git" => azure(org, project, repo),
            [git, repo] if *git == "_git" => azure(org, repo, repo),
            _ => None,
        };
    }
    None
}

/// The Azure DevOps repository a remote addresses, for a project whose code host
/// is *pinned* to Azure DevOps: every shape [`parse_remote`] accepts, plus the
/// same path shapes on any host — an SSH host alias (`azure-work:v3/o/p/r`) or
/// a proxy (`https://git.corp/o/p/_git/r`) — which detection leaves unplaced.
/// Only the org-in-path shapes qualify: a legacy `{org}.visualstudio.com` path
/// behind an alias no longer says which organization it belongs to.
pub fn parse_azure_remote(url: &str) -> Option<AzureRepo> {
    match parse_remote(url) {
        Some(RemoteForge::AzureDevOps(repo)) => return Some(repo),
        Some(RemoteForge::GitHub) => return None,
        None => {}
    }
    let (_, path) = split_host_path(url)?;
    let segments = path_segments(&path);
    let found = match segments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [v, org, project, repo] if v.eq_ignore_ascii_case("v3") => azure(org, project, repo),
        // A proxy may mount the service under a prefix of its own.
        [.., org, project, "_git", repo] => azure(org, project, repo),
        [.., org, "_git", repo] => azure(org, repo, repo),
        _ => None,
    };
    match found? {
        RemoteForge::AzureDevOps(repo) => Some(repo),
        RemoteForge::GitHub => None,
    }
}

/// A URL path's non-empty segments, percent-decoded, query and fragment dropped.
fn path_segments(path: &str) -> Vec<String> {
    path.split(['?', '#'])
        .next()
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(decode_segment)
        .collect()
}

fn azure(org: &str, project: &str, repo: &str) -> Option<RemoteForge> {
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if org.is_empty() || project.is_empty() || repo.is_empty() {
        return None;
    }
    Some(RemoteForge::AzureDevOps(AzureRepo {
        org: org.to_string(),
        project: project.to_string(),
        repo: repo.to_string(),
    }))
}

/// Split a URL (`scheme://[user@]host[:port]/path`) or an scp-like SSH address
/// (`[user@]host:path`) into its lowercased host and its path.
fn split_host_path(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host_port = authority.rsplit('@').next().unwrap_or(authority);
            let host = host_port.split(':').next().unwrap_or(host_port);
            (host, path)
        }
        None => {
            let (user_host, path) = url.split_once(':')?;
            (user_host.rsplit('@').next().unwrap_or(user_host), path)
        }
    };
    (!host.is_empty()).then(|| (host.to_ascii_lowercase(), path.to_string()))
}

/// Percent-decode one path segment (`My%20Project` → `My Project`). Invalid
/// escapes are kept literally — a remote URL is user input.
pub(crate) fn decode_segment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Percent-encode one URL path segment: everything but RFC 3986 unreserved
/// characters.
pub(crate) fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn az(org: &str, project: &str, repo: &str) -> Option<RemoteForge> {
        Some(RemoteForge::AzureDevOps(AzureRepo {
            org: org.into(),
            project: project.into(),
            repo: repo.into(),
        }))
    }

    #[test]
    fn recognizes_github_remotes() {
        for url in [
            "https://github.com/Sigma-studios/usine.git",
            "git@github.com:Sigma-studios/usine.git",
            "ssh://git@github.com/Sigma-studios/usine",
            "ssh://git@ssh.github.com:443/Sigma-studios/usine.git",
        ] {
            assert_eq!(parse_remote(url), Some(RemoteForge::GitHub), "{url}");
        }
    }

    #[test]
    fn recognizes_every_documented_azure_shape() {
        let want = az("fabrikam", "Fiber Tests", "FiberRepo");
        for url in [
            "https://dev.azure.com/fabrikam/Fiber%20Tests/_git/FiberRepo",
            "https://fabrikam@dev.azure.com/fabrikam/Fiber%20Tests/_git/FiberRepo",
            "https://fabrikam.visualstudio.com/Fiber%20Tests/_git/FiberRepo",
            "https://fabrikam.visualstudio.com/DefaultCollection/Fiber%20Tests/_git/FiberRepo",
            "git@ssh.dev.azure.com:v3/fabrikam/Fiber%20Tests/FiberRepo",
            "ssh://git@ssh.dev.azure.com/v3/fabrikam/Fiber%20Tests/FiberRepo",
            "ssh://git@ssh.dev.azure.com:22/v3/fabrikam/Fiber%20Tests/FiberRepo",
            "fabrikam@vs-ssh.visualstudio.com:v3/fabrikam/Fiber%20Tests/FiberRepo",
            "https://dev.azure.com/fabrikam/Fiber%20Tests/_git/FiberRepo/",
        ] {
            assert_eq!(parse_remote(url), want, "{url}");
        }
    }

    #[test]
    fn a_repo_named_after_its_project_may_omit_the_project() {
        assert_eq!(
            parse_remote("https://dev.azure.com/fabrikam/_git/2016_10_31"),
            az("fabrikam", "2016_10_31", "2016_10_31")
        );
        assert_eq!(
            parse_remote("https://fabrikam.visualstudio.com/_git/Web"),
            az("fabrikam", "Web", "Web")
        );
    }

    #[test]
    fn unknown_hosts_and_junk_are_none() {
        for url in [
            "",
            "https://gitlab.com/o/r.git",
            "git@github.example.corp:o/r.git",
            "https://dev.azure.com/fabrikam",
            "https://dev.azure.com/fabrikam/project/_wiki/x",
            "/local/path/to/repo",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn a_pinned_azure_remote_is_read_on_any_host() {
        let want = Some(AzureRepo {
            org: "fabrikam".into(),
            project: "Fiber Tests".into(),
            repo: "FiberRepo".into(),
        });
        for url in [
            "git@azure-work:v3/fabrikam/Fiber%20Tests/FiberRepo",
            "azure-work:v3/fabrikam/Fiber%20Tests/FiberRepo.git",
            "ssh://git@azure-work:2222/v3/fabrikam/Fiber%20Tests/FiberRepo",
            "https://git.corp.example/fabrikam/Fiber%20Tests/_git/FiberRepo",
            "https://git.corp.example/azure/fabrikam/Fiber%20Tests/_git/FiberRepo",
            "https://dev.azure.com/fabrikam/Fiber%20Tests/_git/FiberRepo",
        ] {
            assert_eq!(parse_azure_remote(url), want, "{url}");
        }
        assert_eq!(
            parse_azure_remote("https://git.corp.example/fabrikam/_git/Web"),
            Some(AzureRepo {
                org: "fabrikam".into(),
                project: "Web".into(),
                repo: "Web".into(),
            })
        );
        for url in [
            "git@github.com:o/r.git",
            "git@work-gh:o/r.git",
            "https://git.corp.example/o/r",
            "https://git.corp.example/_git/r",
            "/local/path/to/repo",
        ] {
            assert_eq!(parse_azure_remote(url), None, "{url}");
        }
    }

    #[test]
    fn urls_are_re_encoded_from_decoded_names() {
        let repo = AzureRepo {
            org: "fabrikam".into(),
            project: "Fiber Tests".into(),
            repo: "Fiber Repo".into(),
        };
        assert_eq!(
            repo.git_api(),
            "https://dev.azure.com/fabrikam/Fiber%20Tests/_apis/git/repositories/Fiber%20Repo"
        );
        assert_eq!(
            repo.pr_web_url(7),
            "https://dev.azure.com/fabrikam/Fiber%20Tests/_git/Fiber%20Repo/pullrequest/7"
        );
    }

    #[test]
    fn bad_escapes_survive_decoding() {
        assert_eq!(decode_segment("100%"), "100%");
        assert_eq!(decode_segment("a%zzb"), "a%zzb");
        assert_eq!(decode_segment("a%2Fb"), "a/b");
    }
}
