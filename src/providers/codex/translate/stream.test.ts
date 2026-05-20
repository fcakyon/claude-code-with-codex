import { describe, expect, it } from "bun:test";
import { translateStream } from "./stream.ts";

const silentLog = {
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
  child: () => silentLog,
};

function sse(type: string, payload: Record<string, unknown>): string {
  return `data: ${JSON.stringify({ type, ...payload })}\n\n`;
}

function upstreamFromChunks(chunks: string[], advanceMs = 0): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let index = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (index >= chunks.length) {
        controller.close();
        return;
      }
      now += advanceMs;
      controller.enqueue(encoder.encode(chunks[index++]));
    },
  });
}

function abortingUpstream(err: Error): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      controller.error(err);
    },
  });
}

async function collect(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let out = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) return out;
    out += decoder.decode(value, { stream: true });
  }
}

let now = 0;
const realNow = Date.now;

describe("translateStream", () => {
  it("emits keepalive pings while Read arguments are buffered", async () => {
    Date.now = () => now;
    try {
      const chunks = [
        sse("response.output_item.added", {
          output_index: 0,
          item: { type: "function_call", call_id: "call_read", name: "Read" },
        }),
        sse("response.function_call_arguments.delta", { output_index: 0, delta: "{\"file_path\"" }),
        sse("response.function_call_arguments.delta", { output_index: 0, delta: ":\"/tmp/a\"}" }),
        sse("response.output_item.done", {
          output_index: 0,
          item: { type: "function_call", arguments: "{\"file_path\":\"/tmp/a\"}" },
        }),
        sse("response.completed", { response: { usage: {} } }),
      ];

      const output = await collect(
        translateStream(upstreamFromChunks(chunks, 16_000), {
          messageId: "msg_1",
          model: "gpt-5.5",
          log: silentLog,
        }),
      );

      expect(output).toContain("event: content_block_start");
      expect(output).toContain("event: content_block_delta");
      expect(output.match(/event: ping/g)?.length).toBeGreaterThanOrEqual(2);
      expect(output).toContain("event: message_stop");
    } finally {
      Date.now = realNow;
      now = 0;
    }
  });

  it("treats aborted upstream reads as cancellation", async () => {
    const abort = new AbortController();
    abort.abort();
    const err = new DOMException("The connection was closed.", "AbortError");

    const output = await collect(
      translateStream(abortingUpstream(err), {
        messageId: "msg_1",
        model: "gpt-5.5",
        log: silentLog,
        signal: abort.signal,
      }),
    );

    expect(output).not.toContain("event: error");
  });
});
