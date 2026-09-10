//! Bounded native discovery with account- and endpoint-scoped disk fallback.
use crate::{
    config,
    model_catalog::ModelCatalog,
    providers::{codex, grok, kimi},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BYTES: usize = 2 * 1024 * 1024;
// Discovery protocol version, independent of this proxy's release identity.
const CODEX_CLIENT_VERSION: &str = "0.154.0";

struct Source {
    provider: &'static str,
    url: String,
    account: String,
    headers: reqwest::header::HeaderMap,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    source: String,
    fetched_at: u64,
    models: Vec<String>,
}

fn account_key(access: &str, account: Option<&str>) -> String {
    let subject = access
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v.get("sub").and_then(Value::as_str).map(str::to_owned));
    // Hash opaque tokens when no stable identity is available. Never reuse a
    // catalogue across unknown accounts, and never persist bearer credentials.
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}:{}",
            account.unwrap_or_default(),
            subject
                .as_deref()
                .unwrap_or(if account.is_some() { "" } else { access })
        ))
    )
}

fn models_url(base: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/responses").unwrap_or(path);
    url.set_path(&format!("{path}/models"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn source(provider: &'static str) -> Result<Source> {
    let mut headers = reqwest::header::HeaderMap::new();
    let (base, access, account) = match provider {
        "codex" => {
            let auth = codex::auth::token_store::file_store()
                .load_auth()?
                .context("no login")?;
            headers.insert("originator", "codex_cli_rs".parse()?);
            if let Some(id) = &auth.account_id {
                headers.insert("chatgpt-account-id", id.parse()?);
            }
            (
                config::codex_base_url(codex::auth::constants::CODEX_API_ENDPOINT),
                auth.access,
                auth.account_id,
            )
        }
        "kimi" => {
            let auth = kimi::auth::token_store::file_store()
                .load_auth()?
                .context("no login")?;
            headers.insert(
                "user-agent",
                config::kimi_user_agent("KimiCLI/1.37.0").parse()?,
            );
            (config::kimi_base_url(), auth.access, auth.user_id)
        }
        "grok" => {
            let auth = grok::auth::token_store::file_store()
                .load_auth()?
                .context("no login")?;
            headers.insert("x-xai-token-auth", "xai-grok-cli".parse()?);
            headers.insert("x-grok-client-identifier", "grok-shell".parse()?);
            headers.insert(
                "x-grok-client-version",
                config::grok_client_version().parse()?,
            );
            (config::grok_base_url(), auth.access, None)
        }
        _ => bail!("discovery unavailable"),
    };
    if access.is_empty() {
        bail!("empty access token");
    }
    let account = account_key(&access, account.as_deref());
    headers.insert("authorization", format!("Bearer {access}").parse()?);
    headers.insert("accept", "application/json".parse()?);
    let mut url = models_url(&base)?;
    if provider == "codex" {
        url.push_str(&format!("?client_version={CODEX_CLIENT_VERSION}"));
    }
    Ok(Source {
        provider,
        url,
        account,
        headers,
    })
}

fn valid_models(models: &[String]) -> bool {
    !models.is_empty()
        && models.iter().all(|m| {
            !m.is_empty()
                && m.len() <= 256
                && !m.chars().any(char::is_whitespace)
                && !m.chars().any(char::is_control)
        })
}

fn parse_models(provider: &str, value: Value) -> Result<Vec<String>> {
    let entries = value
        .get(if provider == "codex" {
            "models"
        } else {
            "data"
        })
        .and_then(Value::as_array)
        .context("invalid model list")?;
    let mut models = Vec::new();
    for entry in entries {
        let id = match provider {
            "codex" => entry.get("slug"),
            "grok" => entry.get("model").or_else(|| entry.get("id")),
            _ => entry.get("id"),
        }
        .and_then(Value::as_str)
        .context("invalid model identifier")?;
        models.push(id.to_owned());
    }
    if !valid_models(&models) {
        bail!("empty or invalid model list");
    }
    models.sort_unstable();
    models.dedup();
    Ok(models)
}

impl Source {
    fn key(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("{}:{}:{}", self.provider, self.url, self.account))
        )
    }

    async fn fetch(&self, client: &reqwest::Client) -> Result<Vec<String>> {
        let mut response = client
            .get(&self.url)
            .headers(self.headers.clone())
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                bail!("catalogue too large");
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_models(self.provider, serde_json::from_slice(&bytes)?)
    }
}

fn read_cache(path: &Path, key: &str) -> Option<Vec<String>> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_BYTES as u64 {
        return None;
    }
    let cache: Cache = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (cache.version == 1 && cache.source == key && valid_models(&cache.models))
        .then_some(cache.models)
}

fn write_cache(path: &Path, key: &str, models: &[String]) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().context("cache parent")?)?;
    let cache = Cache {
        version: 1,
        source: key.into(),
        fetched_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        models: models.into(),
    };
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(&cache)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

async fn load(
    source: Source,
    root: PathBuf,
    client: reqwest::Client,
    timeout: Duration,
) -> Option<Vec<String>> {
    let key = source.key();
    let path = root.join(format!("{}-{key}.json", source.provider));
    let cached = read_cache(&path, &key);
    let logger = crate::logging::create_logger("models");
    let (models, origin) = match tokio::time::timeout(timeout, source.fetch(&client)).await {
        Ok(Ok(models)) => {
            if write_cache(&path, &key, &models).is_err() {
                logger.warn(
                    &format!("{} model cache could not be written", source.provider),
                    None,
                );
            }
            (Some(models), "upstream")
        }
        _ => {
            logger.warn(
                &format!(
                    "{} model refresh failed; using local fallback",
                    source.provider
                ),
                None,
            );
            (cached, "cache")
        }
    };
    logger.info(
        &format!(
            "{} models: {} ({})",
            source.provider,
            models.as_ref().map_or(0, Vec::len),
            if models.is_some() { origin } else { "bundled" }
        ),
        None,
    );
    models
}

pub async fn discover() -> ModelCatalog {
    let mut catalog = ModelCatalog::default();
    let Ok(client) = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return catalog;
    };
    let root = crate::paths::config_dir().join("cache/models");
    let results = futures_util::future::join_all(["codex", "kimi", "grok"].into_iter().map(|provider| {
        let client = client.clone();
        let root = root.clone();
        async move {
            let models = match source(provider) {
                Ok(source) => load(source, root, client, TIMEOUT).await,
                Err(_) => {
                    crate::logging::create_logger("models").info(&format!("{provider} models: bundled (no usable login or discovery configuration)"), None);
                    None
                }
            };
            (provider, models)
        }
    })).await;
    for (provider, models) in results {
        if let Some(models) = models {
            catalog.replace(provider, models);
        }
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    async fn mock(provider: &'static str, body: Value) -> (Source, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/models",
            get(move |headers: axum::http::HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer toy-token");
                Json(body)
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("authorization", "Bearer toy-token".parse().unwrap());
        (
            Source {
                provider,
                url,
                account: "toy-account".into(),
                headers,
            },
            task,
        )
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn successful_refresh_replaces_cache_and_offline_start_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let (source, task) = mock("codex", json!({"models":[{"slug":"gpt-toy-new"}]})).await;
        let key = source.key();
        let path = dir.path().join(format!("codex-{key}.json"));
        write_cache(&path, &key, &["gpt-toy-old".into()]).unwrap();
        let models = load(
            Source {
                provider: source.provider,
                url: source.url.clone(),
                account: source.account.clone(),
                headers: source.headers.clone(),
            },
            dir.path().into(),
            client(),
            TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(models, ["gpt-toy-new"]);
        assert_eq!(read_cache(&path, &key).unwrap(), models);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("toy-token")
        );
        task.abort();
        let _ = task.await;
        assert_eq!(
            load(source, dir.path().into(), client(), TIMEOUT)
                .await
                .unwrap(),
            models
        );
    }

    #[tokio::test]
    async fn malformed_or_empty_refresh_preserves_last_good_cache() {
        for body in [
            json!({"data":[]}),
            json!({"data":[{"id":null}]}),
            json!({"wrong":[]}),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (source, task) = mock("kimi", body).await;
            let key = source.key();
            let path = dir.path().join(format!("kimi-{key}.json"));
            write_cache(&path, &key, &["kimi-toy".into()]).unwrap();
            assert_eq!(
                load(source, dir.path().into(), client(), TIMEOUT)
                    .await
                    .unwrap(),
                ["kimi-toy"]
            );
            assert_eq!(read_cache(&path, &key).unwrap(), ["kimi-toy"]);
            task.abort();
        }
    }

    #[tokio::test]
    async fn first_start_and_unwritable_cache_still_use_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "not a directory").unwrap();
        let (source, task) = mock("grok", json!({"data":[{"model":"grok-toy"}]})).await;
        assert_eq!(
            load(source, blocked, client(), TIMEOUT).await.unwrap(),
            ["grok-toy"]
        );
        task.abort();
    }

    #[tokio::test]
    async fn timeout_uses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = Source {
            provider: "codex",
            url: format!("http://{}/models", listener.local_addr().unwrap()),
            account: "toy".into(),
            headers: Default::default(),
        };
        let key = source.key();
        write_cache(
            &dir.path().join(format!("codex-{key}.json")),
            &key,
            &["gpt-toy".into()],
        )
        .unwrap();
        assert_eq!(
            load(
                source,
                dir.path().into(),
                client(),
                Duration::from_millis(20)
            )
            .await
            .unwrap(),
            ["gpt-toy"]
        );
    }

    #[tokio::test]
    async fn one_provider_failure_does_not_discard_another_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let (good, good_task) = mock("codex", json!({"models":[{"slug":"gpt-toy"}]})).await;
        let (bad, bad_task) = mock("grok", json!({"error":"unavailable"})).await;
        let (good_models, bad_models) = tokio::join!(
            load(good, dir.path().into(), client(), TIMEOUT),
            load(bad, dir.path().into(), client(), TIMEOUT),
        );
        assert_eq!(good_models.unwrap(), ["gpt-toy"]);
        assert!(bad_models.is_none());
        good_task.abort();
        bad_task.abort();
    }

    #[test]
    fn corrupt_foreign_and_unknown_version_caches_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(read_cache(&path, "key").is_none());
        write_cache(&path, "other-account", &["toy".into()]).unwrap();
        assert!(read_cache(&path, "key").is_none());
        let mut cache: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cache["version"] = json!(2);
        std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        assert!(read_cache(&path, "other-account").is_none());
    }

    #[test]
    fn account_scope_survives_token_rotation_but_separates_accounts() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"toy-subject"}"#);
        assert_eq!(
            account_key(&format!("a.{payload}.b"), Some("account-a")),
            account_key(&format!("c.{payload}.d"), Some("account-a"))
        );
        assert_ne!(
            account_key("toy", Some("account-a")),
            account_key("toy", Some("account-b"))
        );
    }

    #[test]
    fn native_urls_and_parsers() {
        assert_eq!(
            models_url("https://example.test/backend/codex/responses").unwrap(),
            "https://example.test/backend/codex/models"
        );
        assert_eq!(
            models_url("https://example.test/v1/").unwrap(),
            "https://example.test/v1/models"
        );
        assert_eq!(
            parse_models("kimi", json!({"data":[{"id":"new"},{"id":"new"}]})).unwrap(),
            ["new"]
        );
        assert!(parse_models("codex", json!({"models":[{"slug":"bad\nname"}]})).is_err());
    }
}
