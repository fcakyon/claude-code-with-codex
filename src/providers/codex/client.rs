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
    pub origin: CodexErrorOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorOrigin {
    Http,
    WebSocket,
    Auth,
}

impl CodexError {
    pub fn new(status: u16, message: String) -> Self {
        Self {
            status,
            message,
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        }
    }
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
// Header builder
// ---------------------------------------------------------------------------

pub fn build_codex_headers(
    auth: &StoredAuth,
    ctx: &RequestContext,
) -> Result<http::HeaderMap, CodexError> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        "application/json".parse().map_err(|_| CodexError {
            status: 500,
            message: "Failed to parse content-type header".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        })?,
    );
    headers.insert(
        http::header::ACCEPT,
        "text/event-stream".parse().map_err(|_| CodexError {
            status: 500,
            message: "Failed to parse accept header".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        })?,
    );
    let bearer = format!("Bearer {}", auth.access);
    headers.insert(
        http::header::AUTHORIZATION,
        bearer.parse().map_err(|_| CodexError {
            status: 500,
            message: "Failed to parse authorization header".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        })?,
    );
    let originator = config::codex_originator(ORIGINATOR);
    headers.insert(
        "originator",
        originator.parse().map_err(|_| CodexError {
            status: 500,
            message: "Failed to parse originator header".to_string(),
            detail: None,
            retry_after: None,
            origin: CodexErrorOrigin::Http,
        })?,
    );
    headers.insert("openai-beta", "responses=experimental".parse().unwrap());
    if let Some(ref account_id) = auth.account_id {
        headers.insert("ChatGPT-Account-Id", account_id.parse().unwrap());
    }
    if let Some(ref session_id) = ctx.session_id {
        headers.insert("session_id", session_id.parse().unwrap());
        headers.insert("x-client-request-id", session_id.parse().unwrap());
        let window_id = format!("{session_id}:0");
        headers.insert("x-codex-window-id", window_id.parse().unwrap());
    }
    let user_agent =
        config::codex_user_agent(&format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION")));
    if !user_agent.is_empty() {
        headers.insert(http::header::USER_AGENT, user_agent.parse().unwrap());
    }
    Ok(headers)
}

// ---------------------------------------------------------------------------
// WebSocket request shaping
// ---------------------------------------------------------------------------

pub fn build_websocket_request(
    body: &ResponsesRequest,
    continuation: Option<&super::continuation::ContinuationCandidate>,
) -> serde_json::Value {
    let mut payload = serde_json::to_value(body).unwrap_or_default();
    let obj = payload.as_object_mut().expect("request must be an object");

    // Omit the stream field for WebSocket transport
    obj.remove("stream");

    // Apply continuation if available
    if let Some(candidate) = continuation {
        if let Some(ref prev_id) = candidate.previous_response_id {
            obj.insert(
                "previous_response_id".to_string(),
                serde_json::json!(prev_id),
            );
        }
        if let Some(ref delta) = candidate.input_delta {
            obj.insert(
                "input".to_string(),
                serde_json::to_value(delta).unwrap_or_default(),
            );
        }
    }

    payload
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
    #[allow(dead_code)]
    header_timeout_retries: u32,
}

impl Default for CodexHttpClient {
    fn default() -> Self {
        Self::new()
    }
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
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<CodexResponse, CodexError> {
        use super::continuation::clear_continuation;
        use crate::config::CodexTransport;

        let mut auth = self.auth_manager.get_auth().map_err(|e| CodexError {
            status: 401,
            message: "Auth error".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            origin: CodexErrorOrigin::Auth,
        })?;

        let transport = crate::config::codex_transport();

        for transport_attempt in 0..=3u32 {
            let result = match transport {
                CodexTransport::Http => {
                    let body_json = serde_json::to_string(body).map_err(|e| CodexError {
                        status: 500,
                        message: "Failed to serialize request".to_string(),
                        detail: Some(e.to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    })?;
                    self.attempt_post_http(&auth, &body_json, ctx).await
                }
                CodexTransport::WebSocket => {
                    let ws_headers = build_codex_headers(&auth, ctx)?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(body, continuation);

                    let pool_key = ctx.session_id.as_deref();
                    let pool_key = continuation
                        .and_then(|c| c.previous_response_id.as_ref())
                        .and(pool_key);

                    super::websocket::codex_websocket_request(
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        None, // traffic capture
                        pool_key,
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        continuation,
                    )
                    .await
                }
                CodexTransport::Auto => {
                    let ws_headers = build_codex_headers(&auth, ctx)?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(body, continuation);

                    let pool_key = ctx.session_id.as_deref();
                    let pool_key = continuation
                        .and_then(|c| c.previous_response_id.as_ref())
                        .and(pool_key);

                    // Try WebSocket first
                    let ws_result = super::websocket::codex_websocket_request(
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        None,
                        pool_key,
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        continuation,
                    )
                    .await;

                    match ws_result {
                        Ok(response) => Ok(response),
                        Err(err)
                            if err.status == 0
                                && err.detail.as_deref() == Some("websocket_pre_request") =>
                        {
                            // Fall back to HTTP only if WebSocket failed before sending
                            let body_json =
                                serde_json::to_string(body).map_err(|e| CodexError {
                                    status: 500,
                                    message: "Failed to serialize request".to_string(),
                                    detail: Some(e.to_string()),
                                    retry_after: None,
                                    origin: CodexErrorOrigin::Http,
                                })?;
                            self.attempt_post_http(&auth, &body_json, ctx).await
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            match result {
                Ok(response) if response.status == 401 && transport_attempt == 0 => {
                    // First 401: try refresh
                    match self.auth_manager.force_refresh() {
                        Ok(new_auth) => {
                            auth = new_auth;
                            continue;
                        }
                        Err(e) => {
                            return Err(CodexError {
                                status: 401,
                                message: "Unauthorized".to_string(),
                                detail: Some(e.to_string()),
                                retry_after: None,
                                origin: CodexErrorOrigin::Http,
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
                        origin: CodexErrorOrigin::Http,
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
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) => return Ok(response),
                Err(err) if err.detail.as_deref() == Some("previous_response_not_found") => {
                    // Previous response missing: clear continuation and retry once with full request
                    clear_continuation(ctx.session_id.as_deref());
                    super::websocket::invalidate_codex_websocket_pool_key(
                        ctx.session_id.as_deref().unwrap_or(""),
                    );

                    // Retry with the full body (no continuation) through the relevant transport
                    let retry_headers = build_codex_headers(&auth, ctx)?;
                    let full_body_json = serde_json::to_string(body).map_err(|e| CodexError {
                        status: 500,
                        message: "Failed to serialize request".to_string(),
                        detail: Some(e.to_string()),
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    })?;

                    match transport {
                        CodexTransport::Http | CodexTransport::Auto => {
                            return self.attempt_post_http(&auth, &full_body_json, ctx).await;
                        }
                        CodexTransport::WebSocket => {
                            let ws_retry_headers =
                                super::websocket::codex_websocket_headers(&retry_headers);
                            let ws_retry_body = build_websocket_request(body, None);
                            return super::websocket::codex_websocket_request(
                                &self.base_url,
                                &ws_retry_headers,
                                &ws_retry_body,
                                ctx,
                                None,
                                None,
                                super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                                super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                                None,
                            )
                            .await;
                        }
                    }
                }
                Err(err) => {
                    // Determine if retryable
                    let retryable = is_retryable_transport_error(&err);
                    if retryable && transport_attempt < 3 {
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
            origin: CodexErrorOrigin::Http,
        })
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        body_json: &str,
        ctx: &RequestContext,
    ) -> Result<CodexResponse, CodexError> {
        let url = &self.base_url;

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
        let send_fut = req_builder.body(body_json.to_string()).send();
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
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: None,
                        retry_after: None,
                        origin: CodexErrorOrigin::Http,
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
            origin: CodexErrorOrigin::Http,
        };
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
    }

    #[test]
    fn codex_headers_include_session_and_beta() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: Some("acct".into()),
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: Some("s".into()),
            session_seq: None,
            provider: "codex".into(),
        };
        let headers = build_codex_headers(&auth, &ctx).unwrap();
        assert_eq!(
            headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(headers.get("session_id").unwrap(), "s");
    }

    #[test]
    fn codex_headers_omit_session_when_missing() {
        let auth = StoredAuth {
            access: "tok".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
        };
        let headers = build_codex_headers(&auth, &ctx).unwrap();
        assert!(headers.get("session_id").is_none());
        assert!(headers.get("x-client-request-id").is_none());
    }

    #[test]
    fn build_websocket_request_removes_stream() {
        let input = vec![
            super::super::translate::request::ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    super::super::translate::request::ResponsesContentPart::InputText {
                        text: "hello".to_string(),
                    },
                ],
            },
        ];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let payload = build_websocket_request(&req, None);
        assert!(payload.get("stream").is_none());
        assert!(payload.get("previous_response_id").is_none());
    }

    #[test]
    fn build_codex_headers_error_on_empty_access() {
        let auth = StoredAuth {
            access: "".into(),
            refresh: String::new(),
            account_id: None,
            expires: u64::MAX,
        };
        let ctx = RequestContext {
            req_id: "r".into(),
            session_id: None,
            session_seq: None,
            provider: "codex".into(),
        };
        let result = build_codex_headers(&auth, &ctx);
        assert!(
            result.is_ok(),
            "empty access should still produce valid Bearer header"
        );
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
