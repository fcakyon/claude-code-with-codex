import { encodeSseEvent } from "../../../sse.ts";
import type { Logger } from "../../../log.ts";
import type { TrafficCapture } from "../../types.ts";
import { decodeCursorStream, type CursorStreamEvent, type CursorUsage } from "../client.ts";
import type { CursorProto } from "../proto-loader.ts";
import { createCursorSseFramer } from "../sse-framing.ts";
import {
  CursorToolUseXmlParser,
  type RecoveredCursorTextEvent,
  type RecoveredCursorToolUse,
} from "../tool-use-xml.ts";

export interface AnthropicCursorResponse {
  id: string;
  type: "message";
  role: "assistant";
  model: string;
  content: Array<
    | { type: "thinking"; thinking: string; signature: string }
    | { type: "text"; text: string }
    | { type: "tool_use"; id: string; name: string; input: Record<string, unknown> }
  >;
  stop_reason: "end_turn" | "tool_use" | null;
  stop_sequence: null;
  usage: AnthropicUsage;
}

export interface AnthropicUsage {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
}

export function cursorUsageToAnthropic(usage?: CursorUsage): AnthropicUsage {
  const input = toNumber(usage?.inputTokens);
  const output = toNumber(usage?.outputTokens);
  const cacheRead = toNumber(usage?.cacheReadTokens);
  const cacheWrite = toNumber(usage?.cacheWriteTokens);
  return {
    input_tokens: Math.max(0, input - cacheRead - cacheWrite),
    output_tokens: output,
    cache_creation_input_tokens: cacheWrite,
    cache_read_input_tokens: cacheRead,
  };
}

export async function accumulateCursorResponse(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    traffic?: TrafficCapture;
    proto?: CursorProto;
    onSession?: (sessionId: string) => void;
    allowedToolNames?: ReadonlySet<string>;
  },
): Promise<{ response: AnthropicCursorResponse; cursorSessionId?: string }> {
  let thinking = "";
  let usage: CursorUsage | undefined;
  let cursorSessionId: string | undefined;
  const content: AnthropicCursorResponse["content"] = [];
  const toolUseXml = new CursorToolUseXmlParser({ allowedToolNames: opts.allowedToolNames });

  const flushThinking = () => {
    if (!thinking) return;
    content.push({ type: "thinking", thinking, signature: "" });
    thinking = "";
  };

  const appendText = (text: string) => {
    if (!text) return;
    const previous = content.at(-1);
    if (previous?.type === "text") {
      previous.text += text;
    } else {
      content.push({ type: "text", text });
    }
  };

  const applyRecoveredEvent = (event: RecoveredCursorTextEvent) => {
    flushThinking();
    if (event.type === "text") {
      appendText(event.text);
      return;
    }
    content.push({
      type: "tool_use",
      id: event.id,
      name: event.name,
      input: event.input,
    });
    opts.traffic?.writeJsonEvent("041-cursor-xml-tool-use", {
      id: event.id,
      originalId: event.originalId,
      name: event.name,
      inputChars: JSON.stringify(event.input).length,
    });
  };

  for await (const event of decodeCursorStream(upstream, opts.proto, { traffic: opts.traffic, log: opts.log })) {
    opts.traffic?.writeJsonEvent("040-cursor-event", event);
    switch (event.type) {
      case "session":
        cursorSessionId = event.sessionId;
        opts.onSession?.(event.sessionId);
        break;
      case "thinking_delta":
        thinking += event.text;
        break;
      case "text_delta":
        for (const recovered of toolUseXml.push(event.text)) applyRecoveredEvent(recovered);
        break;
      case "usage":
        usage = event.usage;
        break;
      case "end":
        break;
    }
  }
  for (const recovered of toolUseXml.flush()) applyRecoveredEvent(recovered);
  flushThinking();

  opts.log.debug("cursor accumulate finish", {
    contentBlocks: content.length,
    sawRecoveredToolUse: toolUseXml.sawToolUse,
    cursorSessionId,
    usage,
  });

  return {
    cursorSessionId,
    response: {
      id: opts.messageId,
      type: "message",
      role: "assistant",
      model: opts.model,
      content,
      stop_reason: toolUseXml.sawToolUse ? "tool_use" : "end_turn",
      stop_sequence: null,
      usage: cursorUsageToAnthropic(usage),
    },
  };
}

export function translateCursorStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    signal?: AbortSignal;
    traffic?: TrafficCapture;
    proto?: CursorProto;
    onSession?: (sessionId: string) => void;
    allowedToolNames?: ReadonlySet<string>;
  },
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      let closed = false;

      const emit = (event: string, data: unknown) => {
        if (closed || opts.signal?.aborted || controller.desiredSize === null) return false;
        opts.traffic?.writeJsonEvent("050-downstream-event", { event, data });
        controller.enqueue(encoder.encode(encodeSseEvent(event, data)));
        return true;
      };

      const framing = createCursorSseFramer({
        messageId: opts.messageId,
        model: opts.model,
        emit,
        mapUsage: cursorUsageToAnthropic,
      });
      const toolUseXml = new CursorToolUseXmlParser({ allowedToolNames: opts.allowedToolNames });

      const emitRecoveredToolUse = (tool: RecoveredCursorToolUse) => {
        opts.traffic?.writeJsonEvent("041-cursor-xml-tool-use", {
          id: tool.id,
          originalId: tool.originalId,
          name: tool.name,
          inputChars: JSON.stringify(tool.input).length,
        });
        framing.closeOpenBlocks();
        framing.ensureStart();
        const index = framing.nextContentBlockIndex();
        emit("content_block_start", {
          type: "content_block_start",
          index,
          content_block: {
            type: "tool_use",
            id: tool.id,
            name: tool.name,
            input: {},
          },
        });
        emit("content_block_delta", {
          type: "content_block_delta",
          index,
          delta: { type: "input_json_delta", partial_json: JSON.stringify(tool.input) },
        });
        emit("content_block_stop", { type: "content_block_stop", index });
      };

      const applyRecoveredEvent = (event: RecoveredCursorTextEvent) => {
        if (event.type === "text") {
          framing.emitTextDelta(event.text);
          return;
        }
        emitRecoveredToolUse(event);
      };

      try {
        for await (const event of decodeCursorStream(upstream, opts.proto, { traffic: opts.traffic, log: opts.log })) {
          opts.traffic?.writeJsonEvent("040-cursor-event", event);
          if (opts.signal?.aborted) return;
          switch (event.type) {
            case "session":
              opts.onSession?.(event.sessionId);
              break;
            case "thinking_delta":
              framing.emitThinkingDelta(event.text);
              break;
            case "text_delta":
              for (const recovered of toolUseXml.push(event.text)) applyRecoveredEvent(recovered);
              break;
            case "usage":
              framing.recordUsage(event.usage);
              break;
            case "end":
              break;
          }
        }
        for (const recovered of toolUseXml.flush()) applyRecoveredEvent(recovered);
        framing.emitFinalMessage(toolUseXml.sawToolUse ? "tool_use" : "end_turn");
      } catch (err) {
        opts.log.warn("cursor stream error", { err: String(err) });
        framing.emitError(err);
      } finally {
        closed = true;
        try {
          controller.close();
        } catch {
          // Consumer cancellation can close the controller first.
        }
      }
    },
  });
}

function toNumber(value: string | undefined): number {
  if (!value) return 0;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : 0;
}
