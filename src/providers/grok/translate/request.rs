use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;

use crate::anthropic::schema::{Message, MessagesRequest};

#[derive(Debug, Clone, Serialize)]
pub struct GrokResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<GrokInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GrokTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<GrokToolChoice>,
    pub store: bool,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GrokInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<GrokContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum GrokContentPart {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct GrokTool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum GrokToolChoice {
    Auto(String),
    Required(String),
    None(String),
    Function { r#type: String, name: String },
}

pub fn translate_request(
    req: &MessagesRequest,
    model: String,
) -> anyhow::Result<GrokResponsesRequest> {
    reject_unknown_top_level(req)?;
    let mut instructions = parse_system(req.extra.get("system"))?;
    let mut tools = parse_tools(req.extra.get("tools"))?;
    let hosted_search = tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool.kind == "x_search"));
    let force_hosted_search = hosted_search && requests_web_search(req);
    if force_hosted_search {
        tools = Some(vec![GrokTool {
            kind: "x_search".into(),
            name: None,
            description: None,
            parameters: None,
        }]);
    }
    if hosted_search {
        let guidance = "For web searches, use the hosted x_search tool. Do not use shell commands, HTTP clients, or local tools to search the web.";
        instructions = Some(match instructions {
            Some(existing) if !existing.is_empty() => format!("{existing}\n\n{guidance}"),
            _ => guidance.into(),
        });
    }
    let tool_choice = if force_hosted_search {
        Some(GrokToolChoice::Required("required".into()))
    } else {
        parse_tool_choice(req.extra.get("tool_choice"), tools.as_ref())?
    };
    let mut call_ids = HashSet::new();
    let mut input = Vec::new();
    for message in &req.messages {
        parse_message(message, &mut input, &mut call_ids)?;
    }
    Ok(GrokResponsesRequest {
        model,
        instructions,
        input,
        tools,
        tool_choice,
        store: false,
        stream: true,
        max_output_tokens: req.max_tokens,
    })
}

fn requests_web_search(req: &MessagesRequest) -> bool {
    let Some(message) = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
    else {
        return false;
    };
    let text = match &message.content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return false,
    };
    let text = text.to_ascii_lowercase();
    [
        "search online",
        "search the web",
        "web search",
        "look up online",
        "look up on the web",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

fn reject_unknown_top_level(req: &MessagesRequest) -> anyhow::Result<()> {
    for key in req.extra.keys() {
        if ![
            "system",
            "tools",
            "tool_choice",
            "context_management",
            "metadata",
            "output_config",
            "thinking",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "service_tier",
        ]
        .contains(&key.as_str())
        {
            anyhow::bail!("unsupported Grok request field: {key}");
        }
    }
    Ok(())
}

fn parse_system(value: Option<&Value>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    match value {
        Value::String(text) => Ok(Some(text.clone())),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                let object = block
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("system content must contain text blocks"))?;
                if object
                    .keys()
                    .any(|key| !["type", "text", "cache_control"].contains(&key.as_str()))
                    || object.get("type").and_then(Value::as_str) != Some("text")
                    || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported system block");
                }
                let part = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("system text is invalid"))?;
                text.push_str(part);
            }
            Ok(Some(text))
        }
        _ => anyhow::bail!("system must be text"),
    }
}

fn parse_tools(value: Option<&Value>) -> anyhow::Result<Option<Vec<GrokTool>>> {
    let Some(value) = value else { return Ok(None) };
    let tools = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools must be an array"))?;
    let mut names = HashSet::new();
    let mut out = Vec::new();
    for tool in tools {
        let obj = tool
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tool must be an object"))?;
        for key in obj.keys() {
            if !["name", "description", "input_schema"].contains(&key.as_str()) {
                anyhow::bail!("unsupported tool field: {key}");
            }
        }
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("tool name is invalid"))?;
        if !names.insert(name.to_string()) {
            anyhow::bail!("duplicate tool name");
        }
        if name == "WebSearch" {
            out.push(GrokTool {
                kind: "x_search".into(),
                name: None,
                description: None,
                parameters: None,
            });
            continue;
        }
        let parameters = obj
            .get("input_schema")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tool input_schema must be an object"))?;
        out.push(GrokTool {
            kind: "function".to_string(),
            name: Some(name.to_string()),
            description: obj
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: Some(parameters),
        });
    }
    Ok(Some(out))
}

fn parse_tool_choice(
    value: Option<&Value>,
    tools: Option<&Vec<GrokTool>>,
) -> anyhow::Result<Option<GrokToolChoice>> {
    let Some(value) = value else { return Ok(None) };
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tool_choice must be an object"))?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool_choice type is invalid"))?;
    match kind {
        "auto" if obj.len() == 1 => Ok(Some(GrokToolChoice::Auto("auto".into()))),
        "any" if obj.len() == 1 => Ok(Some(GrokToolChoice::Required("required".into()))),
        "none" if obj.len() == 1 => Ok(Some(GrokToolChoice::None("none".into()))),
        "tool" if obj.len() == 2 => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("tool_choice name is invalid"))?;
            if !tools
                .is_some_and(|items| items.iter().any(|tool| tool.name.as_deref() == Some(name)))
            {
                anyhow::bail!("tool_choice references an unknown tool");
            }
            Ok(Some(GrokToolChoice::Function {
                r#type: "function".into(),
                name: name.into(),
            }))
        }
        _ => anyhow::bail!("unsupported tool_choice"),
    }
}

fn parse_message(
    message: &Message,
    out: &mut Vec<GrokInputItem>,
    calls: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if !["system", "user", "assistant"].contains(&message.role.as_str()) {
        anyhow::bail!("unsupported message role");
    }
    let blocks: Vec<Value> = match &message.content {
        Value::String(text) => vec![serde_json::json!({"type":"text", "text":text})],
        Value::Array(items) => items.clone(),
        _ => anyhow::bail!("message content must be text or blocks"),
    };
    let mut content = Vec::new();
    for block in blocks {
        let object = block
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("content block must be an object"))?;
        let typ = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("content block type is invalid"))?;
        match (message.role.as_str(), typ) {
            (_, "thinking") | (_, "redacted_thinking") => {}
            (_, "text") => {
                if object
                    .keys()
                    .any(|key| !["type", "text", "cache_control"].contains(&key.as_str()))
                    || !valid_cache_control(object.get("cache_control"))
                {
                    anyhow::bail!("unsupported text block field");
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("text block is invalid"))?;
                content.push(if message.role == "assistant" {
                    GrokContentPart::OutputText { text: text.into() }
                } else {
                    GrokContentPart::InputText { text: text.into() }
                });
            }
            ("assistant", "tool_use") => {
                if object
                    .keys()
                    .any(|key| !["type", "id", "name", "input"].contains(&key.as_str()))
                    || object.len() != 4
                {
                    anyhow::bail!("unsupported tool_use field");
                }
                flush_message(&message.role, &mut content, out);
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool call id is invalid"))?;
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool call name is invalid"))?;
                let input = object
                    .get("input")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| anyhow::anyhow!("tool call input must be an object"))?;
                if !calls.insert(id.into()) {
                    anyhow::bail!("duplicate tool call id");
                }
                out.push(GrokInputItem::FunctionCall {
                    call_id: id.into(),
                    name: name.into(),
                    arguments: serde_json::to_string(input)?,
                });
            }
            ("user", "tool_result") => {
                if object.keys().any(|key| {
                    ![
                        "type",
                        "tool_use_id",
                        "content",
                        "is_error",
                        "cache_control",
                    ]
                    .contains(&key.as_str())
                }) {
                    anyhow::bail!("unsupported tool_result field");
                }
                if let Some(is_error) = object.get("is_error")
                    && !is_error.is_boolean()
                {
                    anyhow::bail!("tool result is_error must be boolean");
                }
                flush_message(&message.role, &mut content, out);
                let id = object
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("tool result id is invalid"))?;
                if !calls.remove(id) {
                    anyhow::bail!("tool result references an unknown or resolved tool call");
                }
                let value = object
                    .get("content")
                    .ok_or_else(|| anyhow::anyhow!("tool result content is required"))?;
                let output = match value {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .map(|part| {
                            let part = part.as_object().ok_or_else(|| {
                                anyhow::anyhow!("tool result child must be an object")
                            })?;
                            if part.len() != 2
                                || part.get("type").and_then(Value::as_str) != Some("text")
                                || part
                                    .keys()
                                    .any(|key| !["type", "text"].contains(&key.as_str()))
                            {
                                anyhow::bail!("tool result supports exact text children only");
                            }
                            part.get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| anyhow::anyhow!("tool result text is invalid"))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?
                        .join(""),
                    _ => anyhow::bail!("tool result supports text only"),
                };
                out.push(GrokInputItem::FunctionCallOutput {
                    call_id: id.into(),
                    output,
                });
            }
            _ => anyhow::bail!("unsupported content block: {typ}"),
        }
    }
    flush_message(&message.role, &mut content, out);
    Ok(())
}

fn valid_cache_control(value: Option<&Value>) -> bool {
    let Some(value) = value else { return true };
    let Some(object) = value.as_object() else {
        return false;
    };
    object.keys().all(|key| key == "type" || key == "ttl")
        && object.get("type").and_then(Value::as_str) == Some("ephemeral")
        && object
            .get("ttl")
            .is_none_or(|ttl| matches!(ttl.as_str(), Some("5m") | Some("1h")))
}

fn flush_message(role: &str, content: &mut Vec<GrokContentPart>, out: &mut Vec<GrokInputItem>) {
    if !content.is_empty() {
        out.push(GrokInputItem::Message {
            role: role.into(),
            content: std::mem::take(content),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grok_translation_maps_text_and_function_round_trip() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5", "max_tokens":12, "system":"rules",
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"tool","name":"lookup"},
            "messages":[
              {"role":"user","content":"hello"},
              {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{"q":"a"}}]},
              {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"result"}]}
            ]
        })).unwrap();
        let value =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(value["instructions"], "rules");
        assert_eq!(value["input"][1]["type"], "function_call");
        assert_eq!(value["input"][2]["type"], "function_call_output");
        assert_eq!(value["tool_choice"]["type"], "function");
    }
    #[test]
    fn grok_translation_maps_claude_web_search_to_x_search() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"search online for the project"}],
            "tools":[{
                "name":"WebSearch",
                "description":"Search the web",
                "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
            }]
        }))
        .unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(
            translated["tools"],
            serde_json::json!([{"type":"x_search"}])
        );
        assert!(
            translated["instructions"]
                .as_str()
                .unwrap()
                .contains("use the hosted x_search tool")
        );
        assert_eq!(translated["tool_choice"], "required");
    }

    #[test]
    fn grok_translation_accepts_claude_code_context_management() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-composer-2.5-fast",
            "messages":[{"role":"user","content":"hello"}],
            "context_management":{"edits":[{"type":"clear_tool_uses_20250919","trigger":{"type":"input_tokens","value":100000}}]}
        }))
        .unwrap();
        let translated = translate_request(&request, "grok-composer-2.5-fast".into()).unwrap();
        assert_eq!(translated.input.len(), 1);
    }

    #[test]
    fn grok_translation_rejects_unknown_fields() {
        let request: MessagesRequest = serde_json::from_value(
            serde_json::json!({"model":"grok-4.5","messages":[],"unknown_field":true}),
        )
        .unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_accepts_verified_cache_control_without_forwarding_it() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral","ttl":"5m"}}]}]
        })).unwrap();
        let translated =
            serde_json::to_value(translate_request(&request, "grok-4.5".into()).unwrap()).unwrap();
        assert_eq!(translated["instructions"], "rules");
        assert_eq!(translated["input"][0]["content"][0]["text"], "hello");
        assert!(!translated.to_string().contains("cache_control"));
    }

    #[test]
    fn grok_translation_rejects_invalid_cache_control() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"grok-4.5", "messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"persistent"}}]}]
        })).unwrap();
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    fn request_with_blocks(blocks: Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model":"grok-4.5",
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]},
                {"role":"user","content":blocks}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn grok_translation_rejects_unknown_tool_block_fields() {
        let mut request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"ok"}
        ]));
        request.messages[0].content[0]["unknown"] = Value::Bool(true);
        assert!(translate_request(&request, "grok-4.5".into()).is_err());

        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"ok","unknown":true}
        ]));
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }

    #[test]
    fn grok_translation_rejects_malformed_tool_result_children() {
        for child in [
            serde_json::json!("text"),
            serde_json::json!({"text":"ok"}),
            serde_json::json!({"type":"image","text":"ok"}),
            serde_json::json!({"type":"text","text":1}),
            serde_json::json!({"type":"text","text":"ok","unknown":true}),
        ] {
            let request = request_with_blocks(serde_json::json!([
                {"type":"tool_result","tool_use_id":"call_1","content":[child]}
            ]));
            assert!(translate_request(&request, "grok-4.5".into()).is_err());
        }
    }

    #[test]
    fn grok_translation_rejects_duplicate_tool_results() {
        let request = request_with_blocks(serde_json::json!([
            {"type":"tool_result","tool_use_id":"call_1","content":"first"},
            {"type":"tool_result","tool_use_id":"call_1","content":"second"}
        ]));
        assert!(translate_request(&request, "grok-4.5".into()).is_err());
    }
}
