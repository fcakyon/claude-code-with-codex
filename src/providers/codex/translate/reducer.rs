use crate::anthropic::sse::parse_sse_events;

use super::request::ResponsesInputItem;

#[derive(Debug, Clone)]
pub struct UpstreamStreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub diagnostics: Option<UpstreamStreamDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    RateLimit,
    Overloaded,
    Transient,
    Failed,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpstreamStreamDiagnostics {
    pub event_count: usize,
    pub last_event_type: Option<String>,
    pub saw_terminal_event: bool,
    pub open_blocks: Vec<OpenBlockDiagnostic>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenBlockDiagnostic {
    pub output_index: usize,
    pub anthropic_index: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub input_tokens_details_cached: Option<u64>,
    pub output_tokens_details_reasoning: Option<u64>,
}

pub type StopReason = &'static str;
pub const STOP_END_TURN: &str = "end_turn";
pub const STOP_TOOL_USE: &str = "tool_use";
pub const STOP_MAX_TOKENS: &str = "max_tokens";

pub type TerminalType = &'static str;
pub const TERM_COMPLETED: &str = "response.completed";
pub const TERM_INCOMPLETE: &str = "response.incomplete";
pub const TERM_DONE: &str = "response.done";

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextStop {
        index: usize,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolDelta {
        index: usize,
        partial_json: String,
    },
    ToolStop {
        index: usize,
    },
    ToolProgress {
        index: usize,
    },
    Progress,
    WebSearch {
        index: usize,
        result_index: usize,
        id: String,
        query: String,
    },
    Finish {
        stop_reason: StopReason,
        terminal_type: String,
        continuation_eligible: bool,
        usage: Option<CodexUsage>,
        web_search_requests: usize,
        response_id: Option<String>,
        output_items: Vec<ResponsesInputItem>,
    },
}

#[derive(Debug, Clone)]
pub struct FinishMetadata {
    pub continuation_eligible: bool,
    pub response_id: Option<String>,
    pub output_items: Vec<ResponsesInputItem>,
}

enum BlockState {
    Text {
        index: usize,
        text_accum: String,
    },
    Tool {
        index: usize,
        #[allow(dead_code)]
        output_index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

pub fn finish_metadata_from_upstream(
    input: &[u8],
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes(input)?;
    Ok(events.into_iter().rev().find_map(|event| match event {
        ReducerEvent::Finish {
            continuation_eligible,
            response_id,
            output_items,
            ..
        } => Some(FinishMetadata {
            continuation_eligible,
            response_id,
            output_items,
        }),
        _ => None,
    }))
}

pub fn reduce_upstream_bytes(input: &[u8]) -> Result<Vec<ReducerEvent>, UpstreamStreamError> {
    let sse_events = parse_sse_events(input);
    let mut out = Vec::new();

    let mut blocks_by_output_index: std::collections::HashMap<usize, BlockState> =
        std::collections::HashMap::new();
    let mut output_items_by_index: std::collections::BTreeMap<usize, ResponsesInputItem> =
        std::collections::BTreeMap::new();
    let mut item_id_to_output_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut anthropic_index = 0usize;
    let mut saw_tool_use = false;
    let mut final_usage: Option<CodexUsage> = None;
    let mut response_id: Option<String> = None;
    let mut terminal_type: Option<String> = None;
    let mut continuation_eligible = false;
    let mut incomplete = false;
    let mut web_search_requests = 0usize;
    let mut _saw_terminal = false;
    let mut event_count = 0usize;
    let mut last_event_type: Option<String> = None;

    fn capture_output_item(
        output_index: usize,
        state: &BlockState,
        items: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
    ) {
        match state {
            BlockState::Text {
                index: _,
                text_accum,
            } => {
                if text_accum.is_empty() {
                    return;
                }
                items.insert(
                    output_index,
                    ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content: vec![super::request::ResponsesContentPart::OutputText {
                            text: text_accum.clone(),
                        }],
                    },
                );
            }
            BlockState::Tool {
                args_accum,
                name,
                call_id,
                ..
            } => {
                items.insert(
                    output_index,
                    ResponsesInputItem::FunctionCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: args_accum.clone(),
                    },
                );
            }
        }
    }

    for evt in &sse_events {
        let data = evt.data.trim();
        if data.is_empty() {
            continue;
        }

        let p: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let t = p
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        event_count += 1;
        last_event_type = Some(t.clone());

        if t == "codex.rate_limits" {
            if let Some(true) = p
                .get("rate_limits")
                .and_then(|r| r.get("limit_reached"))
                .and_then(|v| v.as_bool())
            {
                let retry_after = p
                    .get("rate_limits")
                    .and_then(|r| r.get("primary"))
                    .and_then(|r| r.get("reset_after_seconds"))
                    .and_then(|v| v.as_f64());
                return Err(UpstreamStreamError {
                    kind: UpstreamErrorKind::RateLimit,
                    message: "rate limit reached".to_string(),
                    retry_after_seconds: retry_after.map(|f| f as u64),
                    diagnostics: None,
                });
            }
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "keepalive" {
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.web_search_call.in_progress"
            || t == "response.web_search_call.searching"
            || t == "response.web_search_call.completed"
        {
            out.push(ReducerEvent::Progress);
            continue;
        }

        if t == "response.failed" || t == "response.error" || t == "error" {
            let msg = p
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    p.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("Upstream error");
            let kind = upstream_failure_kind(&p, msg);
            let retry_after = retry_after_from_payload(&p);
            return Err(UpstreamStreamError {
                kind,
                message: msg.to_string(),
                retry_after_seconds: retry_after,
                diagnostics: None,
            });
        }

        if t == "response.output_item.added" {
            let item = match p.get("item") {
                Some(v) => v,
                None => continue,
            };
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "reasoning" {
                continue;
            }
            if item_type == "web_search_call" {
                out.push(ReducerEvent::Progress);
                continue;
            }

            if item_type == "message" {
                let idx = anthropic_index;
                anthropic_index += 1;
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    item_id_to_output_index.insert(id.to_string(), output_index);
                }
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Text {
                        index: idx,
                        text_accum: String::new(),
                    },
                );
                out.push(ReducerEvent::TextStart { index: idx });
                continue;
            }

            if item_type == "function_call" {
                saw_tool_use = true;
                let idx = anthropic_index;
                anthropic_index += 1;
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let buffer_until_done = should_buffer_tool_args(&name);
                blocks_by_output_index.insert(
                    output_index,
                    BlockState::Tool {
                        index: idx,
                        output_index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        had_delta: false,
                        buffer_until_done,
                        emitted_args: false,
                    },
                );
                out.push(ReducerEvent::ToolStart {
                    index: idx,
                    id: call_id,
                    name,
                });
                continue;
            }

            continue;
        }

        if t == "response.output_text.delta" {
            let output_index = p
                .get("output_index")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let item_id = p.get("item_id").and_then(|v| v.as_str());
            let state = if let Some(oi) = output_index {
                blocks_by_output_index.get_mut(&oi)
            } else if let Some(id) = item_id {
                item_id_to_output_index
                    .get(id)
                    .and_then(|oi| blocks_by_output_index.get_mut(oi))
            } else {
                None
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            match state {
                Some(BlockState::Text { index, text_accum }) => {
                    text_accum.push_str(delta);
                    out.push(ReducerEvent::TextDelta {
                        index: *index,
                        text: delta.to_string(),
                    });
                }
                _ => continue,
            }
            continue;
        }

        if t == "response.function_call_arguments.delta" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let delta = p.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            let state = match blocks_by_output_index.get_mut(&output_index) {
                Some(s) => s,
                None => continue,
            };
            match state {
                BlockState::Tool {
                    args_accum,
                    had_delta,
                    buffer_until_done,
                    emitted_args,
                    name: _,
                    ..
                } => {
                    args_accum.push_str(delta);
                    *had_delta = true;

                    // Handle buffered Read tool args
                    if *buffer_until_done {
                        // Only emit progress, not delta
                        if let Some(idx) =
                            blocks_by_output_index
                                .get(&output_index)
                                .and_then(|s| match s {
                                    BlockState::Tool { index, .. } => Some(*index),
                                    _ => None,
                                })
                        {
                            out.push(ReducerEvent::ToolProgress { index: idx });
                        }
                    } else {
                        *emitted_args = true;
                        if let Some(idx) =
                            blocks_by_output_index
                                .get(&output_index)
                                .and_then(|s| match s {
                                    BlockState::Tool { index, .. } => Some(*index),
                                    _ => None,
                                })
                        {
                            out.push(ReducerEvent::ToolDelta {
                                index: idx,
                                partial_json: delta.to_string(),
                            });
                        }
                    }
                }
                _ => continue,
            }
            continue;
        }

        if t == "response.function_call_arguments.done" {
            let output_index = match p.get("output_index").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            if let Some(BlockState::Tool { args_accum, .. }) =
                blocks_by_output_index.get_mut(&output_index)
                && let Some(args) = p.get("arguments").and_then(|v| v.as_str())
                && args_accum.is_empty()
            {
                *args_accum = args.to_string();
            }
            continue;
        }

        if t == "response.output_item.done" {
            let output_index: usize =
                p.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let item = p.get("item");

            // Handle web search call done
            if let Some(item_val) = item
                && item_val.get("type").and_then(|v| v.as_str()) == Some("web_search_call")
            {
                let idx = anthropic_index;
                anthropic_index += 1;
                let result_index = anthropic_index;
                anthropic_index += 1;
                web_search_requests += 1;
                let id_val = item_val.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let id = server_tool_use_id_from_codex_web_search_id(id_val);
                let query = web_search_query(item_val);
                out.push(ReducerEvent::WebSearch {
                    index: idx,
                    result_index,
                    id,
                    query,
                });
                continue;
            }

            let state = blocks_by_output_index.remove(&output_index);
            let mut state = match state {
                Some(s) => s,
                None => continue,
            };

            if let Some(item_val) = item {
                if item_val.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                    continue;
                }
                if let BlockState::Tool {
                    args_accum,
                    name,
                    buffer_until_done,
                    emitted_args,
                    had_delta,
                    index,
                    ..
                } = &mut state
                {
                    if let Some(final_args) = item_val
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        if !args_accum.is_empty() && !*emitted_args {
                            // Already have accum from deltas - skip
                        } else if *had_delta {
                            // Already emitted deltas
                        } else {
                            *args_accum = final_args.to_string();
                        }
                    }

                    if !args_accum.is_empty() {
                        let sanitized = sanitize_tool_args(name, args_accum);
                        *args_accum = sanitized;
                        if *buffer_until_done || !*emitted_args {
                            *emitted_args = true;
                            out.push(ReducerEvent::ToolDelta {
                                index: *index,
                                partial_json: args_accum.clone(),
                            });
                        }
                    }
                }
            }

            capture_output_item(output_index, &state, &mut output_items_by_index);

            match &state {
                BlockState::Text { index, .. } => {
                    out.push(ReducerEvent::TextStop { index: *index });
                }
                BlockState::Tool { index, .. } => {
                    out.push(ReducerEvent::ToolStop { index: *index });
                }
            }
            continue;
        }

        if t == "response.completed" || t == "response.incomplete" || t == "response.done" {
            _saw_terminal = true;
            terminal_type = Some(t.clone());
            response_id = p
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            final_usage = p.get("response").map(parse_codex_usage);
            let reason = p
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str());
            if t == "response.incomplete" || reason.is_some() {
                incomplete = true;
            }
            continuation_eligible =
                (t == "response.completed" || t == "response.done") && !incomplete;
            continue;
        }
    }

    let open_blocks = describe_open_blocks(&blocks_by_output_index);
    if !_saw_terminal || !open_blocks.is_empty() {
        let diagnostics = UpstreamStreamDiagnostics {
            event_count,
            last_event_type,
            saw_terminal_event: _saw_terminal,
            open_blocks,
        };
        return Err(UpstreamStreamError {
            kind: UpstreamErrorKind::Transient,
            message: if diagnostics.saw_terminal_event {
                "upstream stream ended with open Codex output blocks".to_string()
            } else {
                "upstream stream ended before terminal Codex response event".to_string()
            },
            retry_after_seconds: None,
            diagnostics: Some(diagnostics),
        });
    }

    let stop_reason: StopReason = if incomplete {
        STOP_MAX_TOKENS
    } else if saw_tool_use {
        STOP_TOOL_USE
    } else {
        STOP_END_TURN
    };

    let output_items: Vec<ResponsesInputItem> = output_items_by_index.into_values().collect();

    out.push(ReducerEvent::Finish {
        stop_reason,
        terminal_type: terminal_type.unwrap_or_else(|| TERM_INCOMPLETE.to_string()),
        continuation_eligible,
        usage: final_usage,
        web_search_requests,
        response_id,
        output_items,
    });

    Ok(out)
}

fn describe_open_blocks(
    blocks: &std::collections::HashMap<usize, BlockState>,
) -> Vec<OpenBlockDiagnostic> {
    let mut out: Vec<_> = blocks
        .iter()
        .map(|(output_index, state)| match state {
            BlockState::Text { index, text_accum } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "text".to_string(),
                name: None,
                call_id: None,
                text_bytes: Some(text_accum.len()),
                argument_bytes: None,
            },
            BlockState::Tool {
                index,
                call_id,
                name,
                args_accum,
                ..
            } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "tool".to_string(),
                name: Some(name.clone()),
                call_id: Some(call_id.clone()),
                text_bytes: None,
                argument_bytes: Some(args_accum.len()),
            },
        })
        .collect();
    out.sort_by_key(|block| block.output_index);
    out
}

fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return CodexUsage::default(),
    };
    CodexUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
    }
}

fn should_buffer_tool_args(name: &str) -> bool {
    name == "Read"
}

fn sanitize_tool_args(name: &str, args: &str) -> String {
    if name != "Read" || args.is_empty() {
        return args.to_string();
    }
    let parsed: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return args.to_string(),
    };
    // Remove empty "pages" field
    let has_empty_pages = obj
        .get("pages")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.is_empty());
    if !has_empty_pages {
        return args.to_string();
    }
    let mut sanitized = obj.clone();
    sanitized.remove("pages");
    serde_json::to_string(&sanitized).unwrap_or_else(|_| args.to_string())
}

fn web_search_query(item: &serde_json::Value) -> String {
    let action = match item.get("action") {
        Some(v) => v,
        None => return String::new(),
    };
    if let Some(query) = action.get("query").and_then(|v| v.as_str()) {
        return query.to_string();
    }
    if let Some(queries) = action.get("queries").and_then(|v| v.as_array()) {
        for q in queries {
            if let Some(s) = q.as_str() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn server_tool_use_id_from_codex_web_search_id(id: &str) -> String {
    let suffix: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("srvtoolu_{suffix}")
}

fn upstream_failure_kind(payload: &serde_json::Value, message: &str) -> UpstreamErrorKind {
    let status = payload
        .get("status")
        .or_else(|| payload.get("status_code"))
        .and_then(|v| v.as_u64());
    let code = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("code"))
        .or_else(|| payload.get("error").and_then(|e| e.get("code")))
        .and_then(|v| v.as_str());
    let err_type = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("type"))
        .or_else(|| payload.get("error").and_then(|e| e.get("type")))
        .and_then(|v| v.as_str());
    let lower_msg = message.to_lowercase();

    if status == Some(529)
        || code == Some("overloaded_error")
        || err_type == Some("overloaded_error")
        || lower_msg.contains("overloaded")
    {
        return UpstreamErrorKind::Overloaded;
    }

    if (status.is_some_and(|s| (500..600).contains(&s)))
        || code == Some("server_error")
        || code == Some("internal_server_error")
        || code == Some("internal_error")
        || err_type == Some("server_error")
        || err_type == Some("internal_server_error")
        || err_type == Some("internal_error")
        || is_retryable_transport_message(&lower_msg)
    {
        return UpstreamErrorKind::Transient;
    }

    UpstreamErrorKind::Failed
}

fn retry_after_from_payload(payload: &serde_json::Value) -> Option<u64> {
    let raw = payload
        .get("response")
        .and_then(|r| r.get("error"))
        .and_then(|e| e.get("retry_after_seconds"))
        .or_else(|| {
            payload
                .get("error")
                .and_then(|e| e.get("retry_after_seconds"))
        })
        .or_else(|| payload.get("retry_after_seconds"))
        .or_else(|| payload.get("headers").and_then(|h| h.get("retry-after")))
        .or_else(|| payload.get("headers").and_then(|h| h.get("Retry-After")));
    let value = match raw {
        Some(v) if v.is_number() => v.as_f64(),
        Some(v) if v.is_string() => v.as_str().and_then(|s| s.parse::<f64>().ok()),
        _ => None,
    };
    value.map(|f| f as u64)
}

fn is_retryable_transport_message(msg: &str) -> bool {
    msg.contains("you can retry your request")
        || msg.contains("socket connection was closed unexpectedly")
        || msg.contains("connection closed unexpectedly")
        || msg.contains("connection reset")
        || msg.contains("operation timed out")
        || msg.contains("econnreset")
        || msg.contains("epipe")
        || msg.contains("etimedout")
        || msg.contains("und_err_socket")
        || msg.contains("fetch failed")
}

pub fn map_codex_usage_to_anthropic(
    u: &Option<CodexUsage>,
    web_search_requests: Option<usize>,
) -> AnthropicUsage {
    let usage = match u {
        Some(u) => u,
        None => return AnthropicUsage::default(),
    };
    let cached = usage.input_tokens_details_cached.unwrap_or(0);
    let total_input = usage.input_tokens.unwrap_or(0);
    let input_tokens = total_input.saturating_sub(cached);

    let mut result = AnthropicUsage {
        input_tokens,
        output_tokens: usage.output_tokens.unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        server_tool_use: None,
    };

    if let Some(requests) = web_search_requests
        && requests > 0
    {
        result.server_tool_use = Some(WebSearchUsage {
            web_search_requests: requests,
        });
    }

    result
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<WebSearchUsage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebSearchUsage {
    pub web_search_requests: usize,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(type_name: &str, payload: serde_json::Value) -> String {
        let mut obj = payload.as_object().cloned().unwrap_or_default();
        obj.insert("type".into(), json!(type_name));
        format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
    }

    #[test]
    fn reduce_text_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":1}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            response_id,
            usage,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "end_turn");
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert_eq!(usage.as_ref().unwrap().input_tokens, Some(5));
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_tool_use_response() {
        let upstream = format!(
            "{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read"}
                })
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,"delta":"{\"file_path\":"
                })
            ),
            sse(
                "response.function_call_arguments.delta",
                json!({
                    "output_index":0,"delta":"\"/tmp/a\"}"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"function_call","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/tmp/a\"}"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish { stop_reason, .. } = last {
            assert_eq!(*stop_reason, "tool_use");
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_rate_limit_throws() {
        let upstream = sse(
            "codex.rate_limits",
            json!({"rate_limits":{"limit_reached":true,"primary":{"reset_after_seconds":30}}}),
        );
        let result = reduce_upstream_bytes(upstream.as_bytes());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind, UpstreamErrorKind::RateLimit);
    }

    #[test]
    fn reduce_upstream_error_event() {
        let upstream = sse("error", json!({"error":{"message":"upstream failure"}}));
        let result = reduce_upstream_bytes(upstream.as_bytes());
        assert!(result.is_err());
        match result.unwrap_err().kind {
            UpstreamErrorKind::Failed => {}
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn reduce_web_search_output() {
        let upstream = format!(
            "{}{}{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1","action":{"query":"test query"}}
                })
            ),
            sse(
                "response.output_item.added",
                json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":1,"delta":"result"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":3}}
                })
            ),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let has_web_search = out
            .iter()
            .any(|e| matches!(e, ReducerEvent::WebSearch { .. }));
        assert!(has_web_search, "expected WebSearch event");
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            web_search_requests,
            ..
        } = last
        {
            assert_eq!(*web_search_requests, 1);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_missing_terminal_is_error() {
        let upstream = format!(
            "{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"partial"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
        );
        let err = reduce_upstream_bytes(upstream.as_bytes()).unwrap_err();
        assert_eq!(err.kind, UpstreamErrorKind::Transient);
        assert!(err.message.contains("terminal"));
    }

    #[test]
    fn reduce_incomplete_is_max_tokens() {
        let upstream = sse(
            "response.incomplete",
            json!({"response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            stop_reason,
            continuation_eligible,
            ..
        } = last
        {
            assert_eq!(*stop_reason, "max_tokens");
            assert!(!continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn reduce_completed_is_continuation_eligible() {
        let upstream = sse(
            "response.completed",
            json!({"response":{"id":"resp_1","usage":{}}}),
        );
        let out = reduce_upstream_bytes(upstream.as_bytes()).unwrap();
        let last = out.last().unwrap();
        if let ReducerEvent::Finish {
            continuation_eligible,
            ..
        } = last
        {
            assert!(continuation_eligible);
        } else {
            panic!("expected Finish");
        }
    }

    #[test]
    fn finish_metadata_extracts_continuation_state() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let metadata = finish_metadata_from_upstream(upstream.as_bytes())
            .unwrap()
            .unwrap();
        assert!(metadata.continuation_eligible);
        assert_eq!(metadata.response_id.as_deref(), Some("resp_1"));
        assert_eq!(metadata.output_items.len(), 1);
    }

    #[test]
    fn sanitize_tool_args_removes_empty_pages() {
        let args = r#"{"file_path":"/tmp/a","pages":""}"#;
        let sanitized = sanitize_tool_args("Read", args);
        let parsed: serde_json::Value = serde_json::from_str(&sanitized).unwrap();
        assert!(parsed.get("pages").is_none());
        assert_eq!(
            parsed.get("file_path").and_then(|v| v.as_str()),
            Some("/tmp/a")
        );
    }

    #[test]
    fn map_usage_subtracts_cached() {
        let usage = CodexUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            input_tokens_details_cached: Some(20),
            output_tokens_details_reasoning: None,
        };
        let mapped = map_codex_usage_to_anthropic(&Some(usage), None);
        assert_eq!(mapped.input_tokens, 80);
        assert_eq!(mapped.output_tokens, 50);
        assert_eq!(mapped.cache_read_input_tokens, 20);
    }
}
