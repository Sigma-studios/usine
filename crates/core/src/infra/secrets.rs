//! Secrets kept in the OS keychain (macOS Keychain, Linux Secret Service,
//! Windows Credential Manager) — never in the database, events or logs.
//!
//! Today that is one value: the Anthropic API key Claude runs fall back to
//! once the plan's usage limit is hit (see `executor/billing.rs`). Keychain
//! failures are logged without the value and read as "no key", which keeps
//! the fallback off rather than breaking a run.

use std::fmt;

/// The keychain service every usine secret is filed under (the app's bundle id).
const SERVICE: &str = "dev.usine.usine";
/// The account name of the Anthropic API key under [`SERVICE`].
const ANTHROPIC_KEY_ACCOUNT: &str = "anthropic-api-key";

/// A secret string whose `Debug`/`Display` never print the value, so it can sit
/// in a `#[derive(Debug)]` config without leaking into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        SecretString(value.into())
    }

    /// The raw value — only for handing to the child process environment.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

fn entry() -> Option<keyring::Entry> {
    match keyring::Entry::new(SERVICE, ANTHROPIC_KEY_ACCOUNT) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!("keychain unavailable: {e}");
            None
        }
    }
}

/// The saved Anthropic API key, if any. A keychain error reads as `None`.
pub fn get_anthropic_key() -> Option<SecretString> {
    match entry()?.get_password() {
        Ok(key) if !key.trim().is_empty() => Some(SecretString(key.trim().to_string())),
        Ok(_) | Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!("couldn't read the Anthropic API key from the keychain: {e}");
            None
        }
    }
}

/// Save (or replace) the Anthropic API key.
pub fn set_anthropic_key(key: &str) -> Result<(), String> {
    let entry = entry().ok_or_else(|| "the OS keychain is unavailable".to_string())?;
    entry
        .set_password(key.trim())
        .map_err(|e| format!("couldn't save the key to the keychain: {e}"))
}

/// Remove the saved Anthropic API key; removing a missing one is fine.
pub fn delete_anthropic_key() -> Result<(), String> {
    let Some(entry) = entry() else {
        return Ok(());
    };
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("couldn't remove the key from the keychain: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_its_value() {
        let s = SecretString::new("sk-ant-api03-secret");
        assert_eq!(format!("{s:?}"), "SecretString(***)");
        assert_eq!(format!("{s}"), "***");
        assert!(!format!("{:?}", Some(s.clone())).contains("secret"));
        assert_eq!(s.expose(), "sk-ant-api03-secret");
    }
}
