use super::reducer::{Reducer, ReducerEvent};
use crate::anthropic::sse::{SseEvent, encode_sse_event};

pub const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct SseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

impl SseDecoder {
    pub fn push(&mut self, input: &[u8]) -> anyhow::Result<Vec<SseEvent>> {
        let mut events = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("Grok SSE stream ended with an incomplete frame")
        }
    }

    fn push_byte(&mut self, byte: u8) -> anyhow::Result<()> {
        if self.frame.len() >= MAX_SSE_FRAME_BYTES {
            anyhow::bail!("Grok SSE frame exceeds the size limit");
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> anyhow::Result<()> {
        if self.frame.len() == self.line_start {
            if !self.frame.is_empty() {
                events.push(parse_frame(&self.frame)?);
            }
            self.frame.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.push_byte(b'\n')?;
        self.line_start = self.frame.len();
        Ok(())
    }
}

fn parse_frame(frame: &[u8]) -> anyhow::Result<SseEvent> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| anyhow::anyhow!("Grok SSE frame contains invalid UTF-8"))?;
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        anyhow::bail!("Grok SSE frame lacks data")
    }
    Ok(SseEvent {
        event,
        data: data.join("\n"),
    })
}

pub struct StreamTranslator {
    message_id: String,
    model: String,
    started: bool,
    finished: bool,
}

pub struct LiveStreamTranslator {
    decoder: SseDecoder,
    reducer: Reducer,
    renderer: StreamTranslator,
}

impl LiveStreamTranslator {
    pub fn new(message_id: String, model: String) -> Self {
        Self {
            decoder: SseDecoder::default(),
            reducer: Reducer::default(),
            renderer: StreamTranslator::new(message_id, model),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        for event in self.decoder.push(chunk)? {
            let value = serde_json::from_str(&event.data)
                .map_err(|_| anyhow::anyhow!("malformed Grok SSE event"))?;
            out.extend(self.renderer.render(self.reducer.push(value)?)?);
        }
        Ok(out)
    }

    pub fn finish(mut self) -> anyhow::Result<()> {
        self.decoder.finish()?;
        if !self.reducer.finished() {
            anyhow::bail!("Grok stream ended without completion");
        }
        Ok(())
    }
}

impl StreamTranslator {
    pub fn new(message_id: String, model: String) -> Self {
        Self {
            message_id,
            model,
            started: false,
            finished: false,
        }
    }

    pub fn render(&mut self, events: Vec<ReducerEvent>) -> anyhow::Result<Vec<u8>> {
        if self.finished && !events.is_empty() {
            anyhow::bail!("event after terminal completion");
        }
        let mut out = Vec::new();
        for event in events {
            if !self.started
                && matches!(
                    event,
                    ReducerEvent::ThinkingStart(_)
                        | ReducerEvent::TextStart(_)
                        | ReducerEvent::ToolStart(_, _, _)
                        | ReducerEvent::Finish { .. }
                )
            {
                self.started = true;
                emit(
                    &mut out,
                    "message_start",
                    serde_json::json!({"type":"message_start","message":{"id":self.message_id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
                );
            }
            if matches!(event, ReducerEvent::Finish { .. }) {
                self.finished = true;
            }
            render(&mut out, event);
        }
        Ok(out)
    }
}

pub fn translate_stream_bytes(
    upstream: &[u8],
    message_id: &str,
    model: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut decoder = SseDecoder::default();
    let mut reducer = Reducer::default();
    let mut translator = StreamTranslator::new(message_id.into(), model.into());
    let mut out = Vec::new();
    for event in decoder.push(upstream)? {
        let value = serde_json::from_str(&event.data)
            .map_err(|_| anyhow::anyhow!("malformed Grok SSE event"))?;
        out.extend(translator.render(reducer.push(value)?)?);
    }
    decoder.finish()?;
    if !reducer.finished() {
        anyhow::bail!("Grok stream ended without completion");
    }
    Ok(out)
}

pub fn stream_error() -> Vec<u8> {
    let data = serde_json::json!({"type":"error","error":{"type":"api_error","message":"Grok stream is invalid"}});
    encode_sse_event(Some("error"), &data.to_string())
}

fn emit(out: &mut Vec<u8>, event: &str, data: serde_json::Value) {
    out.extend(encode_sse_event(Some(event), &data.to_string()));
}

fn render(out: &mut Vec<u8>, event: ReducerEvent) {
    match event {
        ReducerEvent::ThinkingStart(i) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        ReducerEvent::ThinkingDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"thinking_delta","thinking":t}}),
        ),
        ReducerEvent::ThinkingStop(i) | ReducerEvent::TextStop(i) | ReducerEvent::ToolStop(i) => {
            emit(
                out,
                "content_block_stop",
                serde_json::json!({"type":"content_block_stop","index":i}),
            )
        }
        ReducerEvent::TextStart(i) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"text","text":""}}),
        ),
        ReducerEvent::TextDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":t}}),
        ),
        ReducerEvent::ToolStart(i, id, name) => emit(
            out,
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        ),
        ReducerEvent::ToolDelta(i, t) => emit(
            out,
            "content_block_delta",
            serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":t}}),
        ),
        ReducerEvent::Finish {
            stop_reason,
            output_tokens,
            ..
        } => {
            emit(
                out,
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":output_tokens}}),
            );
            emit(
                out,
                "message_stop",
                serde_json::json!({"type":"message_stop"}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_accepts_every_boundary_and_line_ending() {
        let input = b": note\r\nevent: ignored\r\ndata: first\r\ndata: second\r\n\r\n";
        let expected = vec![SseEvent {
            event: Some("ignored".into()),
            data: "first\nsecond".into(),
        }];
        for split in 0..=input.len() {
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&input[..split]).unwrap();
            events.extend(decoder.push(&input[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(events, expected);
        }
    }

    #[test]
    fn decoder_requires_terminated_valid_frames_and_bounds_them() {
        assert!(SseDecoder::default().push(b"data: \xff\n\n").is_err());
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: incomplete").unwrap();
        assert!(decoder.finish().is_err());
        let mut decoder = SseDecoder::default();
        let exact = vec![b'x'; MAX_SSE_FRAME_BYTES - b"data: \n".len()];
        assert!(decoder.push(b"data: ").is_ok());
        assert!(decoder.push(&exact).is_ok());
        let events = decoder.push(b"\n\n").unwrap();
        assert_eq!(events[0].data.len(), exact.len());
        decoder.finish().unwrap();
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: ").is_ok());
        assert!(decoder.push(&vec![b'x'; exact.len() + 1]).is_ok());
        assert!(decoder.push(b"\n").is_err());
    }

    #[test]
    fn live_translator_emits_first_event_before_upstream_completion() {
        let mut translator = LiveStreamTranslator::new("msg_1".into(), "grok-4.5".into());
        let output = translator
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n")
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("first"));
    }
}
