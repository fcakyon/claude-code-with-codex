use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::StatusCode;

use super::auth::manager::GrokAuthManager;
use super::auth::token_store::{StoredAuth, file_store};
use super::translate::request::GrokResponsesRequest;

const DEFAULT_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const MAX_BUFFERED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct GrokClient {
    client: Arc<reqwest::Client>,
    auth: Arc<GrokAuthManager<crate::auth::FileAuthStore<StoredAuth>>>,
    url: String,
    client_version: String,
}

pub struct GrokResponse {
    response: reqwest::Response,
}
pub struct GrokError {
    pub status: StatusCode,
    pub retry_after: Option<String>,
    pub message: String,
}

impl GrokResponse {
    pub fn into_response(self) -> reqwest::Response {
        self.response
    }

    pub fn into_stream(
        self,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, GrokError>> + Send {
        self.response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| GrokError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "Grok upstream stream failed".into(),
            })
        })
    }

    pub async fn into_bytes(self) -> Result<Vec<u8>, GrokError> {
        let mut stream = self.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(GrokError {
                    status: StatusCode::BAD_GATEWAY,
                    retry_after: None,
                    message: "Grok upstream response exceeds the size limit".into(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

impl GrokClient {
    pub fn new(base_url: String, client_version: String) -> anyhow::Result<Self> {
        let client = Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .build()?,
        );
        let auth = Arc::new(GrokAuthManager::new(file_store())?);
        Ok(Self::with_shared(
            url_for(base_url)?,
            client_version,
            client,
            auth,
        ))
    }

    fn with_shared(
        url: String,
        client_version: String,
        client: Arc<reqwest::Client>,
        auth: Arc<GrokAuthManager<crate::auth::FileAuthStore<StoredAuth>>>,
    ) -> Self {
        Self {
            client,
            auth,
            url,
            client_version,
        }
    }

    pub async fn post(&self, body: &GrokResponsesRequest) -> Result<GrokResponse, GrokError> {
        let auth = self.auth.get_auth().await.map_err(auth_error)?;
        let response = self.attempt(&auth.access, body).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let refreshed = self
                .auth
                .force_refresh(&auth.access)
                .await
                .map_err(auth_error)?;
            let replay = self.attempt(&refreshed.access, body).await?;
            if replay.status() == StatusCode::UNAUTHORIZED {
                return Err(auth_error(anyhow::anyhow!("unauthorized")));
            }
            return Ok(GrokResponse { response: replay });
        }
        Ok(GrokResponse { response })
    }

    async fn attempt(
        &self,
        access: &str,
        body: &GrokResponsesRequest,
    ) -> Result<reqwest::Response, GrokError> {
        let response = self
            .client
            .post(&self.url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {access}"))
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-grok-client-version", &self.client_version)
            .json(body)
            .send()
            .await
            .map_err(|_| GrokError {
                status: StatusCode::BAD_GATEWAY,
                retry_after: None,
                message: "Grok upstream request failed".into(),
            })?;
        let status = response.status();
        if !status.is_success() && status != StatusCode::UNAUTHORIZED {
            return Err(GrokError {
                status,
                retry_after: response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
                message: "Grok upstream rejected the request".into(),
            });
        }
        Ok(response)
    }
}

fn url_for(base_url: String) -> anyhow::Result<String> {
    responses_url(&base_url)
}
fn responses_url(base_url: &str) -> anyhow::Result<String> {
    let base_url = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url.trim()
    };
    let mut url = reqwest::Url::parse(base_url)?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        url.set_path(&format!("{path}/responses"));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn auth_error(_: anyhow::Error) -> GrokError {
    GrokError {
        status: StatusCode::UNAUTHORIZED,
        retry_after: None,
        message: "Grok authentication requires official CLI login and proxy import".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::responses_url;
    #[test]
    fn responses_url_appends_responses_to_base_path() {
        assert_eq!(
            responses_url("http://127.0.0.1:8080/v1").unwrap(),
            "http://127.0.0.1:8080/v1/responses"
        );
    }
    #[test]
    fn responses_url_preserves_responses_endpoint() {
        assert_eq!(
            responses_url("https://example.com/custom/responses/").unwrap(),
            "https://example.com/custom/responses"
        );
    }
    #[test]
    fn responses_url_rejects_invalid_url() {
        assert!(responses_url(":invalid").is_err());
    }
}
