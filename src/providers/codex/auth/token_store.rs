use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::jwt::{TokenResponse, extract_account_id, token_exp_ms};
use crate::auth::AuthStorage;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAuth {
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(
        default,
        rename = "accountId",
        alias = "account_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

pub struct CodexTokenStore<S: AuthStorage<StoredAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredAuth>> CodexTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        self.store.load()
    }

    pub fn save_auth(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        self.store.save(value)
    }

    pub fn clear_auth(&self) -> Result<(), anyhow::Error> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }
}

/// Credential source for codex requests: the Codex CLI's `auth.json` (ChatGPT login).
///
/// The proxy never runs its own login. It reads the tokens the Codex CLI already stored
/// and, when it refreshes an expired access token, writes the rotated tokens back so the
/// Codex CLI keeps working. Fields the proxy does not own (auth_mode, OPENAI_API_KEY,
/// tokens.id_token, any future keys) are preserved untouched.
pub struct CodexCliAuthStore {
    path: PathBuf,
}

impl CodexCliAuthStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl AuthStorage<StoredAuth> for CodexCliAuthStore {
    fn load(&self) -> Result<Option<StoredAuth>, anyhow::Error> {
        let raw = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read {}: {err}",
                    self.path.display()
                ));
            }
        };
        let doc: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", self.path.display()))?;
        let Some(tokens) = doc.get("tokens") else {
            return Ok(None);
        };
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if access.is_empty() {
            return Ok(None);
        }
        let refresh = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let account_id = tokens
            .get("account_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                let probe = TokenResponse {
                    id_token: tokens
                        .get("id_token")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    access_token: access.clone(),
                    refresh_token: refresh.clone(),
                    expires_in: None,
                };
                extract_account_id(&probe)
            });
        // The access-token JWT carries the real expiry. If it cannot be parsed, assume a
        // short validity window so a genuinely stale token surfaces as a 401 (handled by
        // force_refresh) rather than being trusted indefinitely.
        let expires = token_exp_ms(&access).unwrap_or_else(|| now_ms() + 3_600_000);
        Ok(Some(StoredAuth {
            access,
            refresh,
            expires,
            account_id,
        }))
    }

    fn save(&self, value: StoredAuth) -> Result<(), anyhow::Error> {
        // Re-read so fields the proxy does not own survive the write.
        let mut doc: serde_json::Value = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        };
        if !doc.is_object() {
            doc = serde_json::json!({});
        }
        let obj = doc.as_object_mut().expect("doc is a json object");
        let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
        if !tokens.is_object() {
            *tokens = serde_json::json!({});
        }
        let tokens = tokens.as_object_mut().expect("tokens is a json object");
        tokens.insert(
            "access_token".into(),
            serde_json::Value::String(value.access),
        );
        tokens.insert(
            "refresh_token".into(),
            serde_json::Value::String(value.refresh),
        );
        if let Some(account_id) = value.account_id {
            tokens.insert("account_id".into(), serde_json::Value::String(account_id));
        }
        obj.insert(
            "last_refresh".into(),
            serde_json::Value::String(now_rfc3339()),
        );
        write_atomic(&self.path, &serde_json::to_vec_pretty(&doc)?)
    }

    fn clear(&self) -> Result<(), anyhow::Error> {
        // No-op: auth.json is owned by the Codex CLI. The proxy must never delete it;
        // a failed refresh should leave the file for the user to `codex login` again.
        Ok(())
    }

    fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

pub type DefaultCodexAuthStore = CodexCliAuthStore;

/// Locate the Codex CLI credential file, honoring `$CODEX_HOME` (and a test override).
pub fn codex_auth_file() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CCP_CODEX_AUTH_FILE") {
        return PathBuf::from(explicit);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(home).join("auth.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".codex").join("auth.json");
    }
    PathBuf::from(".codex").join("auth.json")
}

pub fn file_store() -> CodexTokenStore<DefaultCodexAuthStore> {
    CodexTokenStore::new(CodexCliAuthStore::new(codex_auth_file()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), anyhow::Error> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("auth path has no parent directory"))?;
    std::fs::create_dir_all(dir).ok();
    let tmp = dir.join(format!(".auth.json.tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = file.metadata()?.permissions();
            perm.set_mode(0o600);
            file.set_permissions(perm)?;
        }
        file.write_all(bytes)?;
        file.flush()?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("failed to persist {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use serde_json::json;

    #[test]
    fn stored_auth_reads_account_id_alias() {
        let auth: StoredAuth = serde_json::from_value(json!({
            "access": "a",
            "refresh": "r",
            "expires": 123,
            "accountId": "acct"
        }))
        .unwrap();
        assert_eq!(auth.account_id.as_deref(), Some("acct"));
    }

    #[test]
    fn stored_auth_roundtrip() {
        let store = CodexTokenStore::new(InMemoryAuthStore::new());
        let auth = StoredAuth {
            access: "token".into(),
            refresh: "refresh".into(),
            expires: 9999999999999,
            account_id: Some("acct_1".into()),
        };
        store.save_auth(auth.clone()).unwrap();
        let loaded = store.load_auth().unwrap().unwrap();
        assert_eq!(loaded.access, "token");
        assert_eq!(loaded.account_id.as_deref(), Some("acct_1"));
    }

    fn write_auth_json(dir: &std::path::Path, access: &str) -> PathBuf {
        let path = dir.join("auth.json");
        let doc = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id.tok.sig",
                "access_token": access,
                "refresh_token": "refresh-abc",
                "account_id": "acct_file"
            },
            "last_refresh": "2026-07-13T08:00:00Z"
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        path
    }

    // {"alg":"none"}.{"exp":4102444800}  (exp = year 2100)
    const ACCESS_JWT_2100: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig";

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CodexCliAuthStore::new(dir.path().join("auth.json"));
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn load_maps_tokens_and_account_id_from_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path);
        let auth = store.load().unwrap().unwrap();
        assert_eq!(auth.access, ACCESS_JWT_2100);
        assert_eq!(auth.refresh, "refresh-abc");
        assert_eq!(auth.account_id.as_deref(), Some("acct_file"));
        assert_eq!(auth.expires, 4102444800 * 1000);
    }

    #[test]
    fn save_preserves_unowned_fields_and_updates_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store
            .save(StoredAuth {
                access: "new-access".into(),
                refresh: "new-refresh".into(),
                expires: 0,
                account_id: Some("acct_new".into()),
            })
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["tokens"]["access_token"], "new-access");
        assert_eq!(doc["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(doc["tokens"]["account_id"], "acct_new");
        // Fields the proxy does not own are preserved.
        assert_eq!(doc["auth_mode"], "chatgpt");
        assert!(doc.as_object().unwrap().contains_key("OPENAI_API_KEY"));
        assert_eq!(doc["tokens"]["id_token"], "id.tok.sig");
        assert_ne!(doc["last_refresh"], "2026-07-13T08:00:00Z");
    }

    #[test]
    fn save_sets_0600_permissions() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store
            .save(StoredAuth {
                access: "a".into(),
                refresh: "r".into(),
                expires: 0,
                account_id: None,
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn clear_is_noop_and_keeps_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_auth_json(dir.path(), ACCESS_JWT_2100);
        let store = CodexCliAuthStore::new(path.clone());
        store.clear().unwrap();
        assert!(path.exists(), "clear must not delete the Codex CLI file");
    }
}
