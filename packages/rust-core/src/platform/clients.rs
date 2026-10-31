//! Client keys: one bearer token per agent, team or tool, each with its own name in the usage
//! ledger, an optional daily token budget and an optional admin right.
//!
//! The operator token (`--auth-token-file`) stays all-powerful and unmetered. A client key's
//! usage is the tokens the backend processed for it, output plus the prompt tokens it did not
//! reuse from a prompt cache, per UTC day. Once a day's budget is spent, requests that generate
//! are refused with 429 and a `Retry-After` until the next UTC day; reads still work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

/// One client key, with its token already read.
#[derive(Clone)]
pub struct ClientKey {
    pub name: String,
    pub token: String,
    /// Tokens the backend may process for this key per UTC day; `None` is unmetered.
    pub daily_tokens: Option<u64>,
    /// Whether the key may pull, push, copy, create or delete models and reload the runtime file.
    pub admin: bool,
}

/// Manual so a token never reaches logs.
impl std::fmt::Debug for ClientKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientKey")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("daily_tokens", &self.daily_tokens)
            .field("admin", &self.admin)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysFile {
    #[serde(default)]
    client: Vec<KeyEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyEntry {
    name: String,
    /// Owner-only file holding the token; relative paths are read from the keys file's folder.
    token_file: PathBuf,
    daily_tokens: Option<u64>,
    #[serde(default)]
    admin: bool,
}

/// Read a client keys file (TOML, `[[client]]` tables) and every token file it names.
///
/// # Errors
///
/// Returns an error when the file or a token file cannot be read or is invalid, or two keys share
/// a name or a token.
pub fn load_client_keys(path: &Path) -> Result<Vec<ClientKey>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read client keys file {}", path.display()))?;
    let file: KeysFile = toml::from_str(&text)
        .with_context(|| format!("parse client keys file {}", path.display()))?;
    let folder = path.parent().unwrap_or_else(|| Path::new("."));
    let keys = file
        .client
        .into_iter()
        .map(|entry| {
            let token_path = if entry.token_file.is_absolute() {
                entry.token_file
            } else {
                folder.join(entry.token_file)
            };
            Ok(ClientKey {
                token: crate::auth::read_token_file(&token_path)
                    .with_context(|| format!("client key {}", entry.name))?,
                name: entry.name,
                daily_tokens: entry.daily_tokens,
                admin: entry.admin,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate(&keys, None)?;
    Ok(keys)
}

/// Every key needs a distinct non-empty name and a distinct valid token, also distinct from the
/// operator token.
pub(super) fn validate(keys: &[ClientKey], operator: Option<&str>) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    let mut tokens = std::collections::BTreeSet::new();
    if let Some(operator) = operator {
        tokens.insert(operator);
    }
    for key in keys {
        ensure!(
            !key.name.trim().is_empty() && key.name.len() <= 128,
            "client key names must be 1 to 128 characters"
        );
        ensure!(
            names.insert(key.name.as_str()),
            "client key {} is named twice",
            key.name
        );
        crate::auth::validate_token(&key.token)
            .with_context(|| format!("client key {}", key.name))?;
        ensure!(
            tokens.insert(key.token.as_str()),
            "client key {} reuses another key's token",
            key.name
        );
    }
    Ok(())
}

/// Requests only an admin may make: moving model files and reloading the runtime file.
pub(super) fn needs_admin(method: &axum::http::Method, path: &str) -> bool {
    path.starts_with("/api/blobs/")
        || matches!(
            path,
            "/api/pull" | "/api/push" | "/api/copy" | "/api/delete" | "/api/create"
        )
        || (method == axum::http::Method::POST && path == "/_freellama/v1/config/reload")
}

/// Requests that can make the backend process tokens, which a spent budget refuses.
pub(super) fn spends_tokens(method: &axum::http::Method, path: &str) -> bool {
    if method != axum::http::Method::POST {
        return false;
    }
    match path.strip_prefix("/_freellama/v1/") {
        Some(route) => matches!(route, "tasks" | "task-batches" | "warm"),
        None => !needs_admin(method, path) && path != "/api/show",
    }
}

/// Seconds until the next UTC day, when a spent daily budget renews.
pub(super) fn seconds_until_next_day(now_seconds: u64) -> u64 {
    86_400 - now_seconds % 86_400
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn key(name: &str, token: &str) -> ClientKey {
        ClientKey {
            name: name.into(),
            token: token.into(),
            daily_tokens: None,
            admin: false,
        }
    }

    #[test]
    fn keys_need_distinct_names_and_tokens() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        assert!(validate(&[key("x", &a), key("y", &b)], None).is_ok());
        assert!(validate(&[key("x", &a), key("x", &b)], None).is_err());
        assert!(validate(&[key("x", &a), key("y", &a)], None).is_err());
        assert!(validate(&[key("x", &a)], Some(&a)).is_err());
        assert!(validate(&[key("x", "short")], None).is_err());
    }

    #[test]
    fn generation_spends_tokens_and_model_files_need_admin() {
        assert!(spends_tokens(&Method::POST, "/api/chat"));
        assert!(spends_tokens(&Method::POST, "/v1/chat/completions"));
        assert!(spends_tokens(&Method::POST, "/_freellama/v1/tasks"));
        assert!(!spends_tokens(&Method::POST, "/_freellama/v1/sessions"));
        assert!(!spends_tokens(&Method::GET, "/api/tags"));
        assert!(!spends_tokens(&Method::POST, "/api/show"));
        assert!(!spends_tokens(&Method::POST, "/api/pull"));
        assert!(needs_admin(&Method::POST, "/api/pull"));
        assert!(needs_admin(&Method::DELETE, "/api/delete"));
        assert!(needs_admin(&Method::POST, "/_freellama/v1/config/reload"));
        assert!(!needs_admin(&Method::GET, "/_freellama/v1/config"));
        assert_eq!(seconds_until_next_day(86_400 * 3 + 86_000), 400);
    }

    #[test]
    fn a_keys_file_reads_its_token_files_relative_to_itself() {
        let folder = tempfile::tempdir().unwrap();
        let token = folder.path().join("agent.token");
        std::fs::write(&token, "t".repeat(40)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let file = folder.path().join("keys.toml");
        std::fs::write(
            &file,
            "[[client]]\nname = \"agent\"\ntoken_file = \"agent.token\"\ndaily_tokens = 1000\n",
        )
        .unwrap();
        let keys = load_client_keys(&file).unwrap();
        assert_eq!(keys[0].name, "agent");
        assert_eq!(keys[0].daily_tokens, Some(1000));
        assert!(!keys[0].admin);
        assert!(!format!("{:?}", keys[0]).contains("tttt"));
    }
}
