use std::collections::HashMap;

use super::reducer::{ReducerEvent, reduce_upstream_bytes};
use serde_json::Value;

pub fn accumulate_response(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> anyhow::Result<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut block_positions = HashMap::new();
    let mut stop = "end_turn".to_string();
    let mut input = 0;
    let mut output = 0;
    for event in reduce_upstream_bytes(upstream)? {
        match event {
            ReducerEvent::ThinkingStart(index) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"thinking","thinking":"","signature":""}))
            }
            ReducerEvent::ThinkingDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    block["thinking"] = Value::String(format!(
                        "{}{}",
                        block["thinking"].as_str().unwrap_or(""),
                        text
                    ));
                }
            }
            ReducerEvent::TextStart(index) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"text","text":""}))
            }
            ReducerEvent::TextDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    block["text"] =
                        Value::String(format!("{}{}", block["text"].as_str().unwrap_or(""), text));
                }
            }
            ReducerEvent::ToolStart(index, id, name) => {
                block_positions.insert(index, blocks.len());
                blocks.push(serde_json::json!({"type":"tool_use","id":id,"name":name,"input":{}}))
            }
            ReducerEvent::ToolDelta(index, text) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let raw = format!(
                        "{}{}",
                        block.get("_args").and_then(Value::as_str).unwrap_or(""),
                        text
                    );
                    block["_args"] = Value::String(raw);
                }
            }
            ReducerEvent::ToolStop(index) => {
                if let Some(block) = block_positions
                    .get(&index)
                    .and_then(|position| blocks.get_mut(*position))
                {
                    let raw = block.get("_args").and_then(Value::as_str).unwrap_or("{}");
                    block["input"] = serde_json::from_str(raw)?;
                    block.as_object_mut().unwrap().remove("_args");
                }
            }
            ReducerEvent::Finish {
                stop_reason,
                input_tokens,
                output_tokens,
            } => {
                stop = stop_reason;
                input = input_tokens;
                output = output_tokens;
            }
            _ => {}
        }
    }
    Ok(
        serde_json::json!({"id":message_id,"type":"message","role":"assistant","model":model,"content":blocks,"stop_reason":stop,"stop_sequence":null,"usage":{"input_tokens":input,"output_tokens":output}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulate_response_tracks_two_interleaved_tool_calls() {
        let input = b"data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"first\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"second\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{\\\"value\\\":1}\"}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_2\",\"delta\":\"{\\\"value\\\":2}\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_2\"}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let response = accumulate_response(input, "message", "grok-4.5").unwrap();

        assert_eq!(response["content"][0]["input"]["value"], 1);
        assert_eq!(response["content"][1]["input"]["value"], 2);
    }
}
