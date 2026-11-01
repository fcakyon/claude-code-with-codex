use base64::Engine;
use bytes::Bytes;
use futures_util::StreamExt;
use prost::Message;
use tokio::sync::mpsc;

use crate::config;
use crate::providers::cursor::connect::{
    ConnectFrame, ConnectFrameDecoder, FLAG_END, FLAG_GZIP, encode_connect_frame,
    parse_connect_error,
};
use crate::providers::cursor::model::CursorModelResolution;
use crate::providers::cursor::proto;
use crate::providers::cursor::request::CursorSelectedImage;

/// Upstream response from the Cursor API.
///
/// Contains the raw response bytes (or body bytes for streaming) and the
/// HTTP status.
pub struct CursorUpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub error_detail: Option<String>,
}

impl CursorUpstreamResponse {
    pub fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// How long the reader waits with nothing on the wire before it gives up.
///
/// The agent goes quiet for minutes while Claude Code runs the tool it asked
/// for, so this only has to catch a dead connection.
const RUN_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// One event from a live run.
pub enum LiveEvent {
    Message(Box<proto::AgentServerMessage>),
    End,
    Error(String),
}

/// An open `AgentService/Run` stream.
///
/// `Run` is bidirectional: the request body stays open for the whole turn and
/// every `ExecServerMessage` is answered on the same stream. Closing it (the
/// old code aborted the body after the first pause) makes the agent re-plan
/// from scratch, which re-issues the tool call it was already waiting on.
pub struct LiveRun {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    pub events: mpsc::Receiver<LiveEvent>,
    writer: tokio::task::JoinHandle<()>,
}

impl LiveRun {
    /// Write one client message on the open request body.
    pub async fn send(&self, message: &proto::AgentClientMessage) -> Result<(), CursorError> {
        self.tx
            .send(Ok(encode_connect_frame(message.encode_to_vec(), 0)))
            .await
            .map_err(|_| CursorError::internal("Cursor run already closed"))
    }
}

impl Drop for LiveRun {
    fn drop(&mut self) {
        // Without this the heartbeat task holds the body open forever and the
        // abandoned run keeps a connection (and its tokens) alive upstream.
        self.writer.abort();
    }
}

#[cfg(test)]
impl LiveRun {
    /// A run backed by an in-memory channel: registry tests never touch a
    /// transport.
    pub(crate) fn for_test(events: mpsc::Receiver<LiveEvent>) -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self {
            tx,
            events,
            writer: tokio::spawn(async {}),
        }
    }

    /// A run whose writes land in `tx`, for tests that inspect the frames.
    pub(crate) fn from_channel_for_test(tx: mpsc::Sender<Result<Bytes, std::io::Error>>) -> Self {
        let (_event_tx, events) = mpsc::channel(1);
        Self {
            tx,
            events,
            writer: tokio::spawn(async {}),
        }
    }
}

/// The open HTTP/2 exchange, before either side is turned into events.
struct RunTransport {
    response: reqwest::Response,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    writer: tokio::task::JoinHandle<()>,
}

/// HTTP/2 client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for CursorHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorHttpClient {
    pub fn new() -> Self {
        // One pooled client per transport kind, shared by every request: a client
        // built per request opened a fresh TCP + TLS + HTTP/2 connection each
        // time and never reused one.
        static TLS: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        static CLEARTEXT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

        let base_url = config::cursor_base_url();
        // HTTP/2 prior knowledge for cleartext URLs (mock testing), standard
        // TLS for https URLs.
        let is_cleartext = base_url.starts_with("http://");
        let slot = if is_cleartext { &CLEARTEXT } else { &TLS };
        let client = slot
            .get_or_init(|| {
                let mut builder = reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .tcp_keepalive(std::time::Duration::from_secs(30))
                    // hyper only sends HTTP/2 pings when an interval is set;
                    // the timeout and while_idle knobs alone are inert.
                    .http2_keep_alive_interval(std::time::Duration::from_secs(20))
                    .http2_keep_alive_timeout(std::time::Duration::from_secs(30))
                    .http2_keep_alive_while_idle(true);
                if is_cleartext {
                    builder = builder.http2_prior_knowledge();
                }
                builder.build().expect("CursorHttpClient: reqwest client")
            })
            .clone();

        Self { client, base_url }
    }

    /// Open a live `AgentService/Run` and keep it open.
    ///
    /// The returned handle owns the write side of the request body, so the
    /// caller can answer exec requests on the same stream, and a reader task
    /// that decodes response frames incrementally into `LiveRun::events`.
    pub async fn start_run(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<LiveRun, CursorError> {
        let transport = self.open_run(token, prompt, model, images).await?;
        let status = transport.response.status().as_u16();
        let headers = transport.response.headers().clone();

        if status >= 400 {
            let error_detail = headers
                .get("grpc-message")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let mut stream = transport.response.bytes_stream();
            let mut body = Vec::new();
            while let Some(Ok(chunk)) = stream.next().await {
                body.extend_from_slice(&chunk);
            }
            transport.writer.abort();
            let detail = parse_error_body(&body, &headers).or(error_detail);
            return Err(CursorError::new(status, "Cursor upstream error", detail));
        }

        let (event_tx, event_rx) = mpsc::channel::<LiveEvent>(64);
        let kv_tx = transport.tx.clone();
        tokio::spawn(async move {
            let mut stream = transport.response.bytes_stream();
            let mut decoder = ConnectFrameDecoder::new();
            let mut blobs: std::collections::HashMap<Vec<u8>, Vec<u8>> =
                std::collections::HashMap::new();
            loop {
                let next = tokio::time::timeout(RUN_IDLE_TIMEOUT, stream.next()).await;
                let chunk = match next {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(error))) => {
                        let _ = event_tx
                            .send(LiveEvent::Error(format!("read body: {error}")))
                            .await;
                        return;
                    }
                    Ok(None) => {
                        let _ = event_tx.send(LiveEvent::End).await;
                        return;
                    }
                    Err(_) => {
                        let _ = event_tx
                            .send(LiveEvent::Error(
                                "Cursor upstream went quiet with the run open".to_string(),
                            ))
                            .await;
                        return;
                    }
                };

                let frames = match decoder.push(&chunk) {
                    Ok(frames) => frames,
                    Err(error) => {
                        let _ = event_tx
                            .send(LiveEvent::Error(format!("frame decode: {error}")))
                            .await;
                        return;
                    }
                };

                for frame in frames {
                    if frame.flags & FLAG_END != 0 {
                        // The upstream closed the turn: a payload carries a
                        // Connect error, an empty one is a clean end.
                        let event = match parse_connect_error(&frame.payload) {
                            Some(error) => LiveEvent::Error(error.to_string()),
                            None => LiveEvent::End,
                        };
                        let _ = event_tx.send(event).await;
                        return;
                    }
                    match decode_frame_payload(&frame) {
                        Ok(message) => {
                            // The agent store is answered here rather than by
                            // the pump: a blob the server is waiting on must
                            // land even while the turn is paused on a tool.
                            if let Some(kv) = message.kv_server_message.as_ref()
                                && let Some(reply) = kv_reply(kv, &mut blobs)
                            {
                                if kv_tx
                                    .send(Ok(encode_connect_frame(reply.encode_to_vec(), 0)))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            if event_tx
                                .send(LiveEvent::Message(Box::new(message)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        // Keep reading: one frame the bridge cannot name must
                        // not end a turn that is still running.
                        Err(_) => continue,
                    }
                }
            }
        });

        Ok(LiveRun {
            tx: transport.tx,
            events: event_rx,
            writer: transport.writer,
        })
    }

    /// Send the run request frames and keep the body open with heartbeats.
    async fn open_run(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<RunTransport, CursorError> {
        let resolved = super::model::resolve_cursor_model(model)
            .map_err(|e| CursorError::internal(format!("model resolution: {e}")))?;

        let request_id = uuid::Uuid::new_v4().to_string();
        let frames = build_run_frames(prompt, &resolved, images, &request_id);
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let writer_tx = tx.clone();
        let writer = tokio::spawn(async move {
            for (index, frame) in frames.into_iter().enumerate() {
                if writer_tx.send(Ok(frame)).await.is_err() {
                    return;
                }
                let delay = match index {
                    0 => std::time::Duration::from_millis(1500),
                    1 => std::time::Duration::from_millis(800),
                    _ => std::time::Duration::from_millis(400),
                };
                tokio::time::sleep(delay).await;
            }

            let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
            heartbeat.tick().await;
            loop {
                heartbeat.tick().await;
                if writer_tx.send(Ok(heartbeat_frame())).await.is_err() {
                    return;
                }
            }
        });
        let body =
            reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));

        let url = format!(
            "{}/agent.v1.AgentService/Run",
            self.base_url.trim_end_matches('/')
        );
        let client_version = config::cursor_client_version();

        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip,br")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .header("x-cursor-streaming", "true")
            .header("te", "trailers")
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)?;

        Ok(RunTransport {
            response,
            tx,
            writer,
        })
    }

    /// Run the Cursor agent to the end of its turn and return the raw body.
    ///
    /// Used where the caller needs the untouched frame bytes (traffic capture,
    /// non-streaming replies). Tool rounds need [`Self::start_run`] instead.
    pub async fn run_agent(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<CursorUpstreamResponse, CursorError> {
        let transport = self.open_run(token, prompt, model, images).await?;
        let sender = transport.writer;
        let status = transport.response.status().as_u16();
        let headers = transport.response.headers().clone();
        let error_detail = headers
            .get("grpc-message")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut stream = transport.response.bytes_stream();
        let mut body_bytes = Vec::new();
        let mut received_data = false;

        loop {
            // The upstream stays silent while the model thinks — measured
            // 12/10/2026: a large prompt sent its checkpoint frames, went quiet
            // for more than five seconds, and the old 5s idle cut returned a
            // partial body with no model text (Claude Code showed an empty
            // reply). The stream ends with an explicit END frame, so the idle
            // timeout only guards a truly dead connection.
            let timeout = if received_data {
                std::time::Duration::from_secs(8)
            } else {
                std::time::Duration::from_secs(60)
            };
            match tokio::time::timeout(timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    received_data = true;
                    body_bytes.extend_from_slice(&chunk);
                    if contains_end_frame(&body_bytes) {
                        break;
                    }
                }
                Ok(Some(Err(error))) => {
                    sender.abort();
                    return Err(CursorError::internal(format!("read body: {error}")));
                }
                Ok(None) => break,
                Err(_) if received_data => break,
                Err(_) => {
                    sender.abort();
                    return Err(CursorError::internal(
                        "Cursor upstream timed out before sending a response",
                    ));
                }
            }
        }
        sender.abort();

        if status >= 400 {
            let detail = parse_error_body(&body_bytes, &headers);
            return Err(CursorError::new(status, "Cursor upstream error", detail));
        }

        Ok(CursorUpstreamResponse {
            status,
            body: body_bytes,
            error_detail,
        })
    }
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    encode_varint((field << 3) | 2, &mut out);
    encode_varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn field_string(field: u64, value: &str) -> Vec<u8> {
    field_bytes(field, value.as_bytes())
}

fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(field << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn model_parameter(id: &str, value: &str) -> Vec<u8> {
    let mut parameter = field_string(1, id);
    parameter.extend(field_string(2, value));
    parameter
}

fn model_message(resolved: &CursorModelResolution) -> Vec<u8> {
    let mut out = field_string(1, &resolved.model_id);
    // Grok 4.7 exposes its 500k window through max mode.
    if resolved.model_id == "grok-4.7" {
        out.extend(field_varint(2, 1));
        out.extend(field_bytes(3, &model_parameter("context", "500k")));
    }
    for (id, value) in &resolved.parameters {
        out.extend(field_bytes(3, &model_parameter(id, value)));
    }
    if resolved.send_fast {
        let value = if resolved.fast { "true" } else { "false" };
        out.extend(field_bytes(3, &model_parameter("fast", value)));
    }
    out
}

fn mode_value(resolved: &CursorModelResolution) -> u64 {
    match resolved.mode {
        super::model::CursorAgentMode::Agent => 1,
        super::model::CursorAgentMode::Ask => 2,
        super::model::CursorAgentMode::Plan => 3,
    }
}

fn selected_context(images: &[CursorSelectedImage]) -> Option<Vec<u8>> {
    if images.is_empty() {
        return None;
    }

    let mut context = Vec::new();
    for image in images {
        let data = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .unwrap_or_default();
        let mut selected = field_string(2, &image.uuid);
        selected.extend(field_string(3, &image.path));
        selected.extend(field_string(7, &image.mime_type));
        selected.extend(field_bytes(8, &data));
        context.extend(field_bytes(1, &selected));
    }
    Some(context)
}

fn build_run_frames(
    prompt: &str,
    resolved: &CursorModelResolution,
    images: &[CursorSelectedImage],
    request_id: &str,
) -> Vec<Bytes> {
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let mut user_message = field_string(1, prompt);
    user_message.extend(field_string(2, request_id));
    if let Some(context) = selected_context(images) {
        user_message.extend(field_bytes(3, &context));
    } else {
        user_message.extend(field_bytes(3, &[]));
    }
    user_message.extend(field_varint(4, mode_value(resolved)));

    let action = field_bytes(1, &field_bytes(1, &user_message));
    let mut request = field_bytes(1, &[]);
    request.extend(field_bytes(2, &action));
    request.extend(field_bytes(4, &[]));
    request.extend(field_string(5, &conversation_id));
    request.extend(field_bytes(9, &model_message(resolved)));
    request.extend(field_varint(12, 0));
    request.extend(field_bytes(14, &field_string(1, "default")));
    request.extend(field_bytes(14, &model_message(resolved)));
    request.extend(field_string(16, &conversation_id));
    let first = encode_connect_frame(field_bytes(1, &request), 0);

    let environment = request_context_env().encode_to_vec();
    let context = field_bytes(
        2,
        &field_bytes(
            10,
            &field_bytes(1, &field_bytes(1, &field_bytes(4, &environment))),
        ),
    );

    let mut frames = vec![first, encode_connect_frame(context, 0)];
    frames.push(encode_connect_frame(
        field_bytes(5, &field_string(1, "")),
        0,
    ));
    frames.push(encode_connect_frame(
        field_bytes(3, &field_string(3, "")),
        0,
    ));
    for sequence in 1..=8 {
        let mut marker = field_varint(1, sequence);
        marker.extend(field_string(3, ""));
        frames.push(encode_connect_frame(field_bytes(3, &marker), 0));
    }
    frames
}

/// Answer one agent-store message, keeping the blobs the run was handed.
///
/// The server writes its conversation state here and reads it back on resume;
/// an unanswered message leaves it waiting on its own checkpoint.
fn kv_reply(
    message: &proto::KvServerMessage,
    blobs: &mut std::collections::HashMap<Vec<u8>, Vec<u8>>,
) -> Option<proto::AgentClientMessage> {
    let reply = match message.message.as_ref()? {
        proto::kv_server_message::Message::SetBlobArgs(args) => {
            blobs.insert(args.blob_id.clone(), args.blob_data.clone());
            proto::kv_client_message::Message::SetBlobResult(proto::SetBlobResult {})
        }
        proto::kv_server_message::Message::GetBlobArgs(args) => {
            proto::kv_client_message::Message::GetBlobResult(proto::GetBlobResult {
                blob_data: Some(blobs.get(&args.blob_id).cloned().unwrap_or_default()),
            })
        }
    };
    Some(proto::AgentClientMessage {
        kv_client_message: Some(proto::KvClientMessage {
            id: message.id,
            message: Some(reply),
        }),
        ..Default::default()
    })
}

/// The environment the agent is told it runs in.
///
/// Sent preemptively in the run preamble and again as the answer to
/// `request_context_args`: the server needs a populated context in that answer,
/// and an empty one fails the run with "Failed to get request context
/// (internal)".
pub fn request_context_env() -> proto::RequestContextEnv {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default();
    proto::RequestContextEnv {
        os_version: std::env::consts::OS.to_string(),
        workspace_paths: vec![cwd.clone()],
        shell: if cfg!(windows) { "powershell" } else { "bash" }.to_string(),
        time_zone: "UTC".to_string(),
        project_folder: cwd.clone(),
        sandbox_supported: Some(true),
        sandbox_network_has_defaults: Some(true),
        process_working_directory: Some(cwd),
    }
}

fn heartbeat_frame() -> Bytes {
    encode_connect_frame(field_bytes(7, &[]), 0)
}

fn contains_end_frame(body: &[u8]) -> bool {
    let mut offset = 0;
    while body.len().saturating_sub(offset) >= 5 {
        let length = u32::from_be_bytes([
            body[offset + 1],
            body[offset + 2],
            body[offset + 3],
            body[offset + 4],
        ]) as usize;
        if body.len().saturating_sub(offset) < 5 + length {
            return false;
        }
        if body[offset] & FLAG_END != 0 {
            return true;
        }
        offset += 5 + length;
    }
    false
}

fn parse_error_body(body_bytes: &[u8], _headers: &reqwest::header::HeaderMap) -> Option<String> {
    if body_bytes.len() < 5 {
        return None;
    }
    // Try to parse as Connect end frame with JSON error
    if body_bytes.len() >= 5 {
        let flags = body_bytes[0];
        let len = u32::from_be_bytes([body_bytes[1], body_bytes[2], body_bytes[3], body_bytes[4]])
            as usize;
        if flags & FLAG_END != 0 && body_bytes.len() >= 5 + len {
            let payload = &body_bytes[5..5 + len];
            let err = parse_connect_error(payload);
            if err.is_some() {
                return err.map(|e| e.detail);
            }
        }
    }

    // Try plain text error
    if let Ok(text) = String::from_utf8(body_bytes.to_vec())
        && !text.is_empty()
    {
        return Some(text);
    }
    None
}

/// Decode upstream response bytes into Connect frames containing
/// AgentServerMessage values.
pub fn decode_upstream_frames(body: &[u8]) -> Result<Vec<ConnectFrame>, CursorError> {
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder
        .push(body)
        .map_err(|e| CursorError::internal(format!("frame decode: {e}")))?;
    Ok(frames)
}

/// Decode a single Connect frame payload into an AgentServerMessage.
/// Handles gzip decompression if the FLAG_GZIP bit is set.
pub fn decode_frame_payload(
    frame: &ConnectFrame,
) -> Result<proto::AgentServerMessage, CursorError> {
    let payload = if frame.flags & FLAG_GZIP != 0 {
        super::connect::decode_gzip_frame(&frame.payload)
            .map_err(|e| CursorError::internal(format!("gzip decompress: {e}")))?
    } else {
        frame.payload.to_vec()
    };

    proto::AgentServerMessage::decode(&payload[..])
        .map_err(|e| CursorError::internal(format!("prost decode: {e}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CursorError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: e.to_string(),
            detail: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}

#[cfg(test)]
mod model_message_tests {
    use super::*;

    #[test]
    fn grok_4_7_requests_max_mode_500k() {
        let msg = model_message(&wire("grok-4.7"));
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("grok-4.7"));
        assert!(text.contains("context"));
        assert!(text.contains("500k"));
        assert!(text.contains("fast"));
        assert!(text.contains("false"));
        assert!(msg.windows(2).any(|pair| pair == [0x10, 0x01]));
    }

    #[test]
    fn other_models_do_not_request_max_mode() {
        let msg = model_message(&wire("composer-2.5-fast"));
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("composer-2.5"));
        assert!(text.contains("true"));
        assert!(!text.contains("500k"));
        assert!(!msg.windows(2).any(|pair| pair == [0x10, 0x01]));
    }

    fn wire(model: &str) -> CursorModelResolution {
        crate::providers::cursor::model::resolve_cursor_model(&format!("cursor:{model}")).unwrap()
    }

    #[test]
    fn sonnet_5_5_medium_omits_fast() {
        let msg = model_message(&wire("claude-sonnet-5-5-medium"));
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("claude-sonnet-5-5"));
        assert!(text.contains("reasoning_effort"));
        assert!(text.contains("medium"));
        assert!(text.contains("300k"));
        assert!(!text.contains("fast"));
    }

    #[test]
    fn composer_fast_still_sends_fast() {
        let msg = model_message(&wire("composer-2.5-fast"));
        let text = String::from_utf8_lossy(&msg);
        assert!(text.contains("composer-2.5"));
        assert!(text.contains("fast"));
        assert!(text.contains("true"));
        assert!(!text.contains("300k"));
    }
}
