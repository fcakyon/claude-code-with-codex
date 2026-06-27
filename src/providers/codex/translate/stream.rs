use crate::anthropic::sse::encode_sse_event;

use super::reducer::{ReducerEvent, map_codex_usage_to_anthropic, reduce_upstream_bytes};
use super::web_search_compat::build_web_search_compat_blocks;

pub fn translate_stream_bytes(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    let events = reduce_upstream_bytes(upstream)
        .map_err(|e| anyhow::anyhow!("upstream stream error: {} ({:?})", e.message, e.kind))?;

    let mut out = Vec::new();
    let mut message_started = false;
    let mut open_blocks: std::collections::BTreeMap<usize, OpenBlock> =
        std::collections::BTreeMap::new();
    let mut web_search_events: Vec<ReducerEvent> = Vec::new();
    let mut deferred_content_events: Vec<ReducerEvent> = Vec::new();

    enum OpenBlock {
        Text,
        Tool { id: String, name: String },
    }

    let mut emit = |event: &str, data: &serde_json::Value| -> Result<(), anyhow::Error> {
        out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
        Ok(())
    };

    let mut ensure_message_start = |emit: &mut dyn FnMut(
        &str,
        &serde_json::Value,
    ) -> Result<(), anyhow::Error>|
     -> Result<(), anyhow::Error> {
        if !message_started {
            message_started = true;
            let data = serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                    }
                }
            });
            emit("message_start", &data)?;
        }
        Ok(())
    };

    let is_content_event = |event: &ReducerEvent| -> bool {
        matches!(
            event,
            ReducerEvent::TextStart { .. }
                | ReducerEvent::TextDelta { .. }
                | ReducerEvent::TextStop { .. }
                | ReducerEvent::ToolStart { .. }
                | ReducerEvent::ToolDelta { .. }
                | ReducerEvent::ToolStop { .. }
        )
    };

    for event in &events {
        if matches!(event, ReducerEvent::WebSearch { .. }) {
            web_search_events.push(event.clone());
            continue;
        }
        if !web_search_events.is_empty() && is_content_event(event) {
            deferred_content_events.push(event.clone());
            continue;
        }

        match event {
            ReducerEvent::TextStart { index } => {
                ensure_message_start(&mut emit)?;
                open_blocks.insert(*index, OpenBlock::Text);
                emit(
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                )?;
            }
            ReducerEvent::TextDelta { index, text } => {
                emit(
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                )?;
            }
            ReducerEvent::TextStop { index } => {
                open_blocks.remove(index);
                emit(
                    "content_block_stop",
                    &serde_json::json!({
                        "type": "content_block_stop",
                        "index": index,
                    }),
                )?;
            }
            ReducerEvent::ToolStart { index, id, name } => {
                ensure_message_start(&mut emit)?;
                open_blocks.insert(
                    *index,
                    OpenBlock::Tool {
                        id: id.clone(),
                        name: name.clone(),
                    },
                );
                emit(
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {}
                        }
                    }),
                )?;
            }
            ReducerEvent::ToolDelta {
                index,
                partial_json,
            } => {
                emit(
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial_json
                        }
                    }),
                )?;
            }
            ReducerEvent::ToolStop { index } => {
                open_blocks.remove(index);
                emit(
                    "content_block_stop",
                    &serde_json::json!({
                        "type": "content_block_stop",
                        "index": index,
                    }),
                )?;
            }
            ReducerEvent::ToolProgress { .. } | ReducerEvent::Progress => {
                // These are just progress indicators; emit nothing in buffered mode
            }
            ReducerEvent::Finish {
                stop_reason,
                usage,
                web_search_requests,
                ..
            } => {
                // Emit web search compat blocks
                if !web_search_events.is_empty() {
                    let text_from_deferred: String = deferred_content_events
                        .iter()
                        .filter_map(|e| match e {
                            ReducerEvent::TextDelta { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let compat_blocks =
                        build_web_search_compat_blocks(&web_search_events, &text_from_deferred);
                    for block in &compat_blocks {
                        use super::web_search_compat::WebSearchCompatContent;
                        match &block.content {
                            WebSearchCompatContent::ServerToolUse { id, name, input } => {
                                ensure_message_start(&mut emit)?;
                                emit(
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "server_tool_use",
                                            "id": id,
                                            "name": name,
                                            "input": {}
                                        }
                                    }),
                                )?;
                                emit(
                                    "content_block_delta",
                                    &serde_json::json!({
                                        "type": "content_block_delta",
                                        "index": block.index,
                                        "delta": {
                                            "type": "input_json_delta",
                                            "partial_json": serde_json::to_string(input).unwrap_or_default()
                                        }
                                    }),
                                )?;
                                emit(
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                )?;
                            }
                            WebSearchCompatContent::WebSearchToolResult {
                                tool_use_id,
                                content: results,
                            } => {
                                let result_content: Vec<serde_json::Value> = results
                                    .iter()
                                    .map(|r| {
                                        serde_json::json!({
                                            "type": "web_search_result",
                                            "title": r.title,
                                            "url": r.url,
                                        })
                                    })
                                    .collect();
                                emit(
                                    "content_block_start",
                                    &serde_json::json!({
                                        "type": "content_block_start",
                                        "index": block.index,
                                        "content_block": {
                                            "type": "web_search_tool_result",
                                            "tool_use_id": tool_use_id,
                                            "content": result_content,
                                        }
                                    }),
                                )?;
                                emit(
                                    "content_block_stop",
                                    &serde_json::json!({
                                        "type": "content_block_stop",
                                        "index": block.index,
                                    }),
                                )?;
                            }
                        }
                    }
                }

                // Emit deferred content
                for deferred in &deferred_content_events {
                    match deferred {
                        ReducerEvent::TextStart { index } => {
                            ensure_message_start(&mut emit)?;
                            open_blocks.insert(*index, OpenBlock::Text);
                            emit(
                                "content_block_start",
                                &serde_json::json!({
                                    "type": "content_block_start",
                                    "index": index,
                                    "content_block": {"type": "text", "text": ""}
                                }),
                            )?;
                        }
                        ReducerEvent::TextDelta { index, text } => {
                            emit(
                                "content_block_delta",
                                &serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": {"type": "text_delta", "text": text}
                                }),
                            )?;
                        }
                        ReducerEvent::TextStop { index } => {
                            open_blocks.remove(index);
                            emit(
                                "content_block_stop",
                                &serde_json::json!({
                                    "type": "content_block_stop",
                                    "index": index,
                                }),
                            )?;
                        }
                        ReducerEvent::ToolStart { index, id, name } => {
                            ensure_message_start(&mut emit)?;
                            open_blocks.insert(
                                *index,
                                OpenBlock::Tool {
                                    id: id.clone(),
                                    name: name.clone(),
                                },
                            );
                            emit(
                                "content_block_start",
                                &serde_json::json!({
                                    "type": "content_block_start",
                                    "index": index,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": id,
                                        "name": name,
                                        "input": {}
                                    }
                                }),
                            )?;
                        }
                        ReducerEvent::ToolDelta {
                            index,
                            partial_json,
                        } => {
                            emit(
                                "content_block_delta",
                                &serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": {
                                        "type": "input_json_delta",
                                        "partial_json": partial_json
                                    }
                                }),
                            )?;
                        }
                        ReducerEvent::ToolStop { index } => {
                            open_blocks.remove(index);
                            emit(
                                "content_block_stop",
                                &serde_json::json!({
                                    "type": "content_block_stop",
                                    "index": index,
                                }),
                            )?;
                        }
                        _ => {}
                    }
                }

                ensure_message_start(&mut emit)?;

                let mapped = map_codex_usage_to_anthropic(usage, Some(*web_search_requests));
                emit(
                    "message_delta",
                    &serde_json::json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason,
                            "stop_sequence": null
                        },
                        "usage": mapped,
                    }),
                )?;
                emit("message_stop", &serde_json::json!({"type": "message_stop"}))?;
            }
            _ => {}
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_event(type_name: &str, payload: serde_json::Value) -> String {
        let mut obj = if let serde_json::Value::Object(m) = payload {
            m
        } else {
            return String::new();
        };
        obj.insert("type".into(), serde_json::json!(type_name));
        format!(
            "data: {}\n\n",
            serde_json::to_string(&serde_json::Value::Object(obj)).unwrap()
        )
    }

    #[test]
    fn stream_translates_text_response() {
        let upstream = format!(
            "{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"item_1"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse_event(
                "response.completed",
                serde_json::json!({
                    "response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":1}}
                })
            ),
        );
        let out = String::from_utf8(
            translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap(),
        )
        .unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn stream_translates_web_search_response() {
        let upstream = format!(
            "{}{}{}{}{}{}{}",
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1"}
                })
            ),
            sse_event(
                "response.web_search_call.in_progress",
                serde_json::json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.web_search_call.completed",
                serde_json::json!({
                    "output_index":0,"item_id":"ws_1"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":0,
                    "item":{"type":"web_search_call","id":"ws_1","action":{"query":"test query"}}
                })
            ),
            sse_event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":1,
                    "item":{"type":"message","id":"msg_up"}
                })
            ),
            sse_event(
                "response.output_text.delta",
                serde_json::json!({
                    "output_index":1,"delta":"See [Result](https://result.com)"
                })
            ),
            sse_event(
                "response.output_item.done",
                serde_json::json!({
                    "output_index":1,"item":{"type":"message"}
                })
            ),
        );
        let result = translate_stream_bytes(upstream.as_bytes(), "msg_1", "gpt-5.5").unwrap();
        let out = String::from_utf8(result).unwrap();
        assert!(out.contains("server_tool_use"), "missing server_tool_use");
        assert!(
            out.contains("web_search_tool_result"),
            "missing web_search_tool_result"
        );
    }
}
