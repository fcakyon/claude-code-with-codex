use std::time::Duration;

use crate::config;
use crate::provider::RequestContext;
use crate::retry::{compute_backoff_delay, sleep};

use super::auth::constants::{CODEX_API_ENDPOINT, ORIGINATOR};
use super::auth::manager::CodexAuthManager;
use super::auth::token_store::{StoredAuth, file_store};
use super::translate::request::ResponsesRequest;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex error {}: {}", self.status, self.message)
    }
}

#[derive(Debug)]
pub struct CodexHeaderTimeoutError {
    pub timeout_ms: u64,
}

impl std::fmt::Display for CodexHeaderTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Timed out waiting {}ms for Codex response headers",
            self.timeout_ms
        )
    }
}

#[derive(Debug)]
pub struct CodexTransportError {
    pub message: String,
}

impl std::fmt::Display for CodexTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex transport error: {}", self.message)
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

pub struct CodexResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct CodexHttpClient {
    client: reqwest::Client,
    auth_manager: CodexAuthManager<crate::auth::FileAuthStore<StoredAuth>>,
    base_url: String,
    header_timeout_ms: u64,
    header_timeout_retries: u32,
}

impl CodexHttpClient {
    pub fn new() -> Self {
        let timeout_ms = 60_000;
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_millis(timeout_ms + 10_000))
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: CodexAuthManager::new(file_store()),
            base_url: config::codex_base_url(CODEX_API_ENDPOINT),
            header_timeout_ms: timeout_ms,
            header_timeout_retries: 1,
        }
    }

    pub fn new_with_client(
        client: reqwest::Client,
        auth_manager: CodexAuthManager<crate::auth::FileAuthStore<StoredAuth>>,
        base_url: String,
    ) -> Self {
        Self {
            client,
            auth_manager,
            base_url,
            header_timeout_ms: 60_000,
            header_timeout_retries: 1,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        client: reqwest::Client,
        base_url: String,
        header_timeout_ms: u64,
        header_timeout_retries: u32,
    ) -> Self {
        Self {
            client,
            auth_manager: CodexAuthManager::new(file_store()),
            base_url,
            header_timeout_ms,
            header_timeout_retries,
        }
    }

    pub fn auth_manager(&self) -> &CodexAuthManager<crate::auth::FileAuthStore<StoredAuth>> {
        &self.auth_manager
    }

    pub async fn post_codex(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
    ) -> Result<CodexResponse, CodexError> {
        let mut auth = self.auth_manager.get_auth().map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        // Wrap attempt in retry loop for header timeout and transport errors
        let max_transport_retries = 10u32;
        for transport_attempt in 0..=max_transport_retries {
            // Inner loop for 429 rate limiting
            let result = self.attempt_post(&auth, body, ctx, transport_attempt).await;

            match result {
                Ok(response) if response.status == 401 && transport_attempt == 0 => {
                    // First 401: try refresh
                    match self.auth_manager.force_refresh() {
                        Ok(new_auth) => {
                            auth = new_auth;
                            // Don't increment transport_attempt, retry the same attempt index
                            continue;
                        }
                        Err(e) => {
                            return Err(CodexError {
                                status: 401,
                                message: "Unauthorized".to_string(),
                                detail: Some(e.to_string()),
                                retry_after: None,
                            });
                        }
                    }
                }
                Ok(response) if response.status == 403 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 403,
                        message: "Forbidden".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                    });
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "retry-after")
                        .map(|(_, v)| v.clone());
                    if transport_attempt < 3 {
                        let delay =
                            compute_backoff_delay(transport_attempt, retry_after.as_deref());
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                    });
                }
                Ok(response) => return Ok(response),
                Err(err) => {
                    // Determine if retryable
                    let retryable = is_retryable_transport_error(&err);
                    if retryable && transport_attempt < max_transport_retries {
                        let delay = compute_backoff_delay(transport_attempt, None);
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(CodexError {
            status: 0,
            message: "Max retries exceeded".to_string(),
            detail: None,
            retry_after: None,
        })
    }

    async fn attempt_post(
        &self,
        auth: &StoredAuth,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        _attempt: u32,
    ) -> Result<CodexResponse, CodexError> {
        let url = &self.base_url;
        let body_json = serde_json::to_string(body).map_err(|e| CodexError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
        })?;

        // Build headers
        let mut req_builder = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("Authorization", format!("Bearer {}", auth.access))
            .header("originator", config::codex_originator(ORIGINATOR))
            .header("openai-beta", "responses=experimental");

        if let Some(ref account_id) = auth.account_id {
            req_builder = req_builder.header("ChatGPT-Account-Id", account_id);
        }
        if let Some(ref session_id) = ctx.session_id {
            req_builder = req_builder
                .header("session_id", session_id)
                .header("x-client-request-id", session_id)
                .header("x-codex-window-id", format!("{session_id}:0"));
        }

        let user_agent =
            config::codex_user_agent(&format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION")));
        if !user_agent.is_empty() {
            req_builder = req_builder.header("User-Agent", user_agent);
        }

        // Apply header timeout
        let send_fut = req_builder.body(body_json.clone()).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: None,
                retry_after: None,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: None,
                        retry_after: None,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: None,
                        retry_after: None,
                    }
                }
            })?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let body_bytes = resp.bytes().await.unwrap_or_default().to_vec();

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
        })
    }
}

fn is_retryable_transport_error(err: &CodexError) -> bool {
    err.status == 0
        && (err.message.contains("Timed out waiting")
            || err.message.contains("Transport error")
            || err.message.contains("connection reset")
            || err.message.contains("connection closed")
            || err.message.contains("timed out")
            || err.message.contains("econnreset")
            || err.message.contains("etimedout"))
}

fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("econnreset")
        || msg.contains("etimedout")
        || msg.contains("epipe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_error_display() {
        let err = CodexError {
            status: 429,
            message: "Rate limited".to_string(),
            detail: Some("body".to_string()),
            retry_after: Some("5".to_string()),
        };
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
    }

    #[test]
    fn codex_header_timeout_error_display() {
        let err = CodexHeaderTimeoutError { timeout_ms: 60000 };
        let display = format!("{err}");
        assert!(display.contains("60000"));
    }

    #[test]
    fn codex_transport_error_display() {
        let err = CodexTransportError {
            message: "connection reset".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("connection reset"));
    }
}
