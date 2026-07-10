pub mod auth;
pub mod client;
pub mod count_tokens;
pub mod translate;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::{
    error::json_error,
    schema::{CountTokensResponse, MessagesRequest},
};
use crate::monitor::MonitorHandle;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::registry::GROK_MODELS;

use self::auth::token_store::file_store;
use self::translate::{
    accumulate::accumulate_response,
    model_allowlist::{assert_allowed_model, resolve_model},
    request::translate_request,
    stream::{SseDecoder, StreamTranslator, stream_error},
};

pub struct GrokProvider {
    client: Arc<client::GrokClient>,
}
impl GrokProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(
                client::GrokClient::new(
                    crate::config::grok_base_url(),
                    crate::config::grok_client_version(),
                )
                .expect("Grok transport is unavailable"),
            ),
        }
    }

    pub fn with_client(client: client::GrokClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }
}
impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn name(&self) -> &'static str {
        "grok"
    }
    fn supported_models(&self) -> Vec<String> {
        GROK_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    }
    fn cli(&self) -> &'static dyn CliHandlers {
        &GROK_CLI
    }
    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let resolved = resolve_model(&requested);
        if let Err(error) = assert_allowed_model(&resolved) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let translated = match translate_request(&body, resolved.clone()) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        if let Some(monitor) = &ctx.monitor {
            monitor.model_resolved(&ctx.req_id, &resolved);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self.client.post(&translated).await {
            Ok(response) => response,
            Err(error) => return map_error(error),
        };
        if body.stream {
            stream_response(
                upstream,
                format!("msg_{}", uuid::Uuid::new_v4().simple()),
                requested,
                ctx.monitor.clone(),
                ctx.req_id.clone(),
            )
        } else {
            let upstream_bytes = match upstream.into_bytes().await {
                Ok(bytes) => bytes,
                Err(error) => return map_error(error),
            };
            match accumulate_response(
                &upstream_bytes,
                &format!("msg_{}", uuid::Uuid::new_v4().simple()),
                &requested,
            ) {
                Ok(value) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            value
                                .pointer("/usage/input_tokens")
                                .and_then(|v| v.as_u64()),
                            value
                                .pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    (StatusCode::OK, Json(value)).into_response()
                }
                Err(_) => json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Grok response is invalid",
                ),
            }
        }
    }
    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let requested = body.model.clone().unwrap_or_else(|| "grok-4.5".into());
        let resolved = resolve_model(&requested);
        if let Err(error) = assert_allowed_model(&resolved) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
        let translated = match translate_request(&body, resolved) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
        let tokens = count_tokens::count_tokens(&translated);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

fn stream_response(
    response: client::GrokResponse,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
) -> Response {
    stream_body(response.into_stream(), message_id, model, monitor, req_id)
}

fn stream_body<S>(
    upstream: S,
    message_id: String,
    model: String,
    monitor: Option<MonitorHandle>,
    req_id: String,
) -> Response
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin + Send + 'static,
{
    let state = GrokStreamState {
        upstream,
        decoder: SseDecoder::default(),
        reducer: translate::reducer::Reducer::default(),
        translator: StreamTranslator::new(message_id, model),
        terminal: false,
        error_sent: false,
        monitor,
        req_id,
        bytes: 0,
        chunks: 0,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        state
            .next_output()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(Bytes::from(bytes)), state))
    });
    (
        [
            (http::header::CONTENT_TYPE, "text/event-stream"),
            (http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

struct GrokStreamState<S> {
    upstream: S,
    decoder: SseDecoder,
    reducer: translate::reducer::Reducer,
    translator: StreamTranslator,
    terminal: bool,
    error_sent: bool,
    monitor: Option<MonitorHandle>,
    req_id: String,
    bytes: u64,
    chunks: u64,
}

impl<S> GrokStreamState<S>
where
    S: Stream<Item = Result<Bytes, client::GrokError>> + Unpin,
{
    async fn next_output(&mut self) -> Option<Vec<u8>> {
        if self.terminal {
            return None;
        }
        if self.error_sent {
            self.terminal = true;
            return None;
        }
        loop {
            let chunk = match self.upstream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(_)) => return Some(self.fail()),
                None => {
                    if self.decoder.finish().is_err() || !self.reducer.finished() {
                        return Some(self.fail());
                    }
                    self.terminal = true;
                    return None;
                }
            };
            self.bytes = self.bytes.saturating_add(chunk.len() as u64);
            self.chunks = self.chunks.saturating_add(1);
            if let Some(monitor) = self.monitor.as_ref() {
                monitor.stream_progress(&self.req_id, self.bytes, self.chunks, None, None);
            }
            let events = match self.decoder.push(&chunk) {
                Ok(events) => events,
                Err(_) => return Some(self.fail()),
            };
            let mut out = Vec::new();
            for event in events {
                let value: serde_json::Value = match serde_json::from_str(&event.data) {
                    Ok(value) => value,
                    Err(_) => return Some(self.fail()),
                };
                let reduced = match self.reducer.push(value) {
                    Ok(events) => events,
                    Err(_) => return Some(self.fail()),
                };
                let usage = reduced.iter().find_map(|event| match event {
                    translate::reducer::ReducerEvent::Finish {
                        input_tokens,
                        output_tokens,
                        ..
                    } => Some((*input_tokens, *output_tokens)),
                    _ => None,
                });
                match self.translator.render(reduced) {
                    Ok(bytes) => out.extend(bytes),
                    Err(_) => return Some(self.fail()),
                }
                if let Some((input_tokens, output_tokens)) = usage
                    && let Some(monitor) = self.monitor.as_ref()
                {
                    monitor.usage_updated(&self.req_id, Some(input_tokens), Some(output_tokens));
                }
                if self.reducer.finished() {
                    self.terminal = true;
                    return if out.is_empty() { None } else { Some(out) };
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
    }

    fn fail(&mut self) -> Vec<u8> {
        self.error_sent = true;
        stream_error()
    }
}

fn map_error(error: client::GrokError) -> Response {
    match error.status {
        StatusCode::UNAUTHORIZED => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            error.message,
        ),
        StatusCode::TOO_MANY_REQUESTS => {
            let response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                error.message,
            );
            if let Some(retry_after) = error.retry_after {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        StatusCode::PAYMENT_REQUIRED | StatusCode::FORBIDDEN => {
            json_error(error.status, "permission_error", error.message)
        }
        _ => json_error(StatusCode::BAD_GATEWAY, "api_error", error.message),
    }
}

pub struct GrokCli;
pub static GROK_CLI: GrokCli = GrokCli;
impl CliHandlers for GrokCli {
    fn login(&self) -> anyhow::Result<()> {
        let store = file_store();
        auth::login::login(&store)?;
        println!("Grok authentication saved in {}", store.auth_path());
        Ok(())
    }
    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!("Grok device login is unavailable; use grok auth login")
    }
    fn status(&self) -> anyhow::Result<()> {
        let store = file_store();
        match store.load_auth()? {
            Some(auth) => {
                println!("Auth path: {}", store.auth_path());
                println!("Authenticated: true");
                println!(
                    "Expires in {}s",
                    auth.expires_at_ms.saturating_sub(now_ms()) / 1000
                );
                Ok(())
            }
            None => anyhow::bail!("Not authenticated"),
        }
    }
    fn logout(&self) -> anyhow::Result<()> {
        let store = file_store();
        store.clear_auth()?;
        println!("Grok proxy credentials removed");
        Ok(())
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use crate::monitor::{EndpointKind, MonitorHandle};
    use http_body_util::BodyExt;
    use tokio::sync::mpsc;

    use super::*;

    struct ChannelStream(mpsc::Receiver<Result<Bytes, client::GrokError>>);

    impl Stream for ChannelStream {
        type Item = Result<Bytes, client::GrokError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    #[tokio::test]
    async fn streaming_usage_updates_completed_monitor_request() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "req_1",
            Some("session_1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.provider_selected("req_1", "grok", "grok-4.5", None);
        monitor.request_completed("req_1", 200, None, None);

        let upstream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.output_text.done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}}\n\n",
        ))]);
        let response = stream_body(
            upstream,
            "msg_1".into(),
            "grok-4.5".into(),
            Some(monitor.clone()),
            "req_1".into(),
        );
        let _ = response.into_body().collect().await.unwrap();

        let snapshot = monitor.snapshot();
        let request = snapshot
            .recent
            .iter()
            .find(|request| request.request_id == "req_1")
            .unwrap();
        assert_eq!(request.input_tokens, Some(12));
        assert_eq!(request.output_tokens, Some(3));
        assert!(request.streamed_bytes > 0);
        assert!(request.stream_chunks > 0);
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id.as_deref() == Some("session_1"))
            .unwrap();
        assert_eq!(session.input_tokens, 12);
        assert_eq!(session.output_tokens, 3);
    }

    #[tokio::test]
    async fn downstream_event_arrives_before_upstream_completion() {
        let (tx, rx) = mpsc::channel(2);
        let response = stream_body(
            ChannelStream(rx),
            "msg_1".into(),
            "grok-4.5".into(),
            None,
            "req_1".into(),
        );
        let mut body = response.into_body();

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n",
        )))
        .await
        .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream body waited for upstream completion")
            .expect("downstream body ended before its first event")
            .expect("downstream body frame failed")
            .into_data()
            .expect("first downstream frame was not data");
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains("event: message_start"));
        assert!(first.contains("first"));

        tx.send(Ok(Bytes::from_static(
            b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n",
        )))
        .await
        .unwrap();
        let terminal = tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .expect("downstream completion timed out")
            .expect("downstream completion was missing")
            .expect("downstream completion frame failed")
            .into_data()
            .expect("downstream completion frame was not data");
        assert!(
            String::from_utf8(terminal.to_vec())
                .unwrap()
                .contains("event: message_stop")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), body.frame())
                .await
                .expect("downstream EOF waited for upstream EOF")
                .is_none()
        );
    }
}
