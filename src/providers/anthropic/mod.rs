//! Anthropic passthrough backend.
//!
//! Unlike the other providers, this one performs no translation. Claude Code already
//! speaks the Anthropic Messages API and, when pointed at a custom base URL, forwards
//! its own subscription credentials (`Authorization: Bearer sk-ant-oat...`) plus the
//! `anthropic-beta` flags that drive prompt caching. So the correct behavior is a
//! transparent reverse proxy: relay the original body bytes and headers to
//! api.anthropic.com and stream the response straight back. The proxy holds zero
//! Anthropic credentials and never touches the cache-keyed request prefix.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::MessagesRequest;
use crate::logging::create_logger;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::translate_shared::wrap_reasoning;
use crate::registry::ANTHROPIC_STYLE_ALIASES;

/// Rewrite an outgoing Anthropic request body so it survives a mid-conversation switch
/// away from the codex backend.
///
/// Claude Code stores the codex backend's reconstructed reasoning as `thinking` blocks
/// carrying an empty signature. Anthropic rejects those on replay (400
/// `Invalid signature in thinking block`), so every post-switch turn would otherwise pay
/// a failed round-trip plus Claude Code's strip-and-retry. A native Anthropic turn does
/// carry prior-turn reasoning forward, so instead of dropping it we convert each
/// signature-less `thinking` block into a tagged `text` block: Anthropic accepts text
/// without a signature and the reasoning stays in context. Genuine Anthropic reasoning
/// (a non-empty signature) is left untouched.
///
/// Returns rewritten bytes only when something changed; `None` forwards the body
/// verbatim, keeping the byte-identical cache prefix for pure-Anthropic conversations.
fn sanitize_anthropic_request(raw: &[u8], req_id: &str) -> Option<Vec<u8>> {
    let mut doc: Value = serde_json::from_slice(raw).ok()?;
    let obj = doc.as_object_mut()?;

    detect_hosted_web_search_regression(obj, req_id);

    let messages = obj.get_mut("messages")?.as_array_mut()?;
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut() {
            changed |= rehydrate_unsigned_thinking(block);
        }
    }

    changed.then(|| serde_json::to_vec(&doc).unwrap_or_else(|_| raw.to_vec()))
}

/// Convert one signature-less `thinking` block into a tagged `text` block in place.
/// Returns whether the block was rewritten.
fn rehydrate_unsigned_thinking(block: &mut Value) -> bool {
    let Some(map) = block.as_object() else {
        return false;
    };
    if map.get("type").and_then(Value::as_str) != Some("thinking") {
        return false;
    }
    let signed = map
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|sig| !sig.is_empty());
    if signed {
        return false;
    }
    let reasoning = map.get("thinking").and_then(Value::as_str).unwrap_or("");
    *block = serde_json::json!({
        "type": "text",
        "text": wrap_reasoning(reasoning),
    });
    true
}

/// Regression tripwire. Claude Code drives its `WebSearch` tool through an isolated,
/// history-free inner call, so the hosted `web_search_20250305` tool and its
/// reconstructed `server_tool_use` / `web_search_tool_result` blocks never appear in the
/// outer transcript. If that ever changes (hosted web search reaching a request that
/// already carries assistant history), those blocks would ride the transcript across a
/// model switch and this warning flags it so the assumption can be re-checked.
fn detect_hosted_web_search_regression(obj: &serde_json::Map<String, Value>, req_id: &str) {
    let messages = obj.get("messages").and_then(Value::as_array);
    let has_assistant_history = messages.is_some_and(|ms| {
        ms.iter()
            .any(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
    });
    let hosted_tool = obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|ts| {
            ts.iter()
                .any(|t| t.get("type").and_then(Value::as_str) == Some("web_search_20250305"))
        });
    let reconstructed_block = messages.is_some_and(|ms| {
        ms.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("server_tool_use") | Some("web_search_tool_result")
                        )
                    })
                })
        })
    });

    if (hosted_tool && has_assistant_history) || reconstructed_block {
        let mut fields = serde_json::Map::new();
        fields.insert("reqId".into(), Value::String(req_id.to_string()));
        fields.insert("hostedWebSearchTool".into(), Value::Bool(hosted_tool));
        fields.insert(
            "reconstructedSearchBlock".into(),
            Value::Bool(reconstructed_block),
        );
        create_logger("anthropic").warn("hosted_web_search_in_history", Some(fields));
    }
}

/// Request headers that must not be forwarded to the upstream. Hop-by-hop headers are
/// connection-scoped; `content-length` is recomputed by the client from the body; and
/// `accept-encoding` is dropped so the upstream answers with an identity encoding
/// (this build of reqwest does not decompress, so forwarding a compressed body under a
/// stale `content-encoding` would corrupt it).
fn is_stripped_request_header(name: &str) -> bool {
    matches!(
        name,
        "host" | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "accept-encoding"
    )
}

/// Response headers that must not be relayed back to Claude Code. Hop-by-hop and
/// framing headers are re-derived by axum for the streamed body; `content-encoding`
/// is dropped for symmetry with the identity request above.
fn is_stripped_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "content-encoding"
    )
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build anthropic passthrough client");
        Self {
            client,
            base_url: crate::config::anthropic_base_url(),
        }
    }

    async fn relay(&self, ctx: RequestContext) -> Response {
        let RequestContext {
            req_id,
            monitor,
            passthrough,
            ..
        } = ctx;
        let Some(passthrough) = passthrough else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "anthropic passthrough is missing the original request",
            );
        };

        let url = format!("{}{}", self.base_url, passthrough.path_and_query);
        let mut headers = axum::http::HeaderMap::with_capacity(passthrough.headers.len());
        for (name, value) in passthrough.headers.iter() {
            if is_stripped_request_header(name.as_str()) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }

        if let Some(monitor) = monitor.as_ref() {
            monitor.upstream_started(&req_id);
        }

        // Rehydrate signature-less codex `thinking` blocks so a mid-conversation switch
        // to Anthropic does not 400. Unchanged bodies are forwarded verbatim.
        let outgoing = match sanitize_anthropic_request(&passthrough.raw_body, &req_id) {
            Some(bytes) => reqwest::Body::from(bytes),
            None => reqwest::Body::from(passthrough.raw_body),
        };

        let upstream = self
            .client
            .post(&url)
            .headers(headers)
            .body(outgoing)
            .send()
            .await;

        match upstream {
            Ok(upstream) => {
                let status = upstream.status();
                let mut out_headers =
                    axum::http::HeaderMap::with_capacity(upstream.headers().len());
                for (name, value) in upstream.headers() {
                    if is_stripped_response_header(name.as_str()) {
                        continue;
                    }
                    out_headers.append(name.clone(), value.clone());
                }
                let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
                *response.status_mut() = status;
                *response.headers_mut() = out_headers;
                response
            }
            Err(err) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("anthropic upstream request failed: {err}"),
            ),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        ANTHROPIC_STYLE_ALIASES
            .iter()
            .map(|alias| (*alias).to_string())
            .collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &ANTHROPIC_CLI
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx).await
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx).await
    }
}

pub struct AnthropicCli;
pub static ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!("The Claude backend reuses Claude Code's own login; no separate authentication is required")
    }
    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!("The Claude backend reuses Claude Code's own login; no separate authentication is required")
    }
    fn status(&self) -> anyhow::Result<()> {
        println!("Claude backend: transparent passthrough to api.anthropic.com");
        println!("Auth: forwarded from Claude Code (no proxy credentials stored)");
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        println!("Claude backend stores no credentials; nothing to remove");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::translate_shared::{REASONING_CLOSE, REASONING_OPEN};

    #[test]
    fn strips_hop_by_hop_and_encoding_from_request() {
        assert!(is_stripped_request_header("host"));
        assert!(is_stripped_request_header("content-length"));
        assert!(is_stripped_request_header("accept-encoding"));
        assert!(is_stripped_request_header("connection"));
        // credentials and cache-relevant headers must survive
        assert!(!is_stripped_request_header("authorization"));
        assert!(!is_stripped_request_header("anthropic-beta"));
        assert!(!is_stripped_request_header("anthropic-version"));
        assert!(!is_stripped_request_header("content-type"));
    }

    #[test]
    fn strips_framing_from_response() {
        assert!(is_stripped_response_header("content-length"));
        assert!(is_stripped_response_header("content-encoding"));
        assert!(is_stripped_response_header("transfer-encoding"));
        // rate-limit and request-id headers must reach Claude Code
        assert!(!is_stripped_response_header("content-type"));
        assert!(!is_stripped_response_header("request-id"));
        assert!(!is_stripped_response_header("anthropic-ratelimit-requests-remaining"));
    }

    #[test]
    fn provider_reports_name_and_models() {
        let provider = AnthropicProvider::new();
        assert_eq!(provider.name(), "anthropic");
        assert!(provider.supported_models().iter().any(|m| m == "opus"));
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn unsigned_thinking_becomes_tagged_text() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "codex reasoning", "signature": ""},
                    {"type": "text", "text": "391"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let out = sanitize_anthropic_request(&raw, "req1").expect("should rewrite");
        let doc = parse(&out);
        let blocks = doc["messages"][1]["content"].as_array().unwrap();
        // the thinking block is gone, replaced by tagged text; the real answer survives
        assert!(blocks.iter().all(|b| b["type"] != "thinking"));
        let tagged = blocks[0]["text"].as_str().unwrap();
        assert!(tagged.starts_with(REASONING_OPEN), "{tagged}");
        assert!(tagged.contains("codex reasoning"), "{tagged}");
        assert!(tagged.ends_with(REASONING_CLOSE), "{tagged}");
        assert_eq!(blocks[1]["text"], "391");
    }

    #[test]
    fn signed_thinking_is_forwarded_verbatim() {
        // A genuine Anthropic reasoning block (non-empty signature) must not be touched,
        // so a pure-Anthropic conversation keeps its byte-identical cache prefix.
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "opus reasoning", "signature": "abc123"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        assert!(sanitize_anthropic_request(&raw, "req2").is_none());
    }

    #[test]
    fn missing_signature_is_treated_as_unsigned() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "r"}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let out = sanitize_anthropic_request(&raw, "req3").expect("should rewrite");
        assert_eq!(parse(&out)["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn plain_request_is_forwarded_verbatim() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        assert!(sanitize_anthropic_request(&raw, "req4").is_none());
    }

    #[test]
    fn rewrite_is_deterministic() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "same", "signature": ""}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let a = sanitize_anthropic_request(&raw, "r").unwrap();
        let b = sanitize_anthropic_request(&raw, "r").unwrap();
        assert_eq!(a, b, "rewrite must be byte-stable to preserve the cache prefix");
    }

    #[test]
    fn non_json_body_is_forwarded_verbatim() {
        assert!(sanitize_anthropic_request(b"not json", "req5").is_none());
    }
}
