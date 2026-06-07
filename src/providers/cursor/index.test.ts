import { afterEach, describe, expect, it } from "bun:test";
import { cursorProvider, createCursorProvider } from "./index.ts";
import type { RequestContext } from "../types.ts";
import { encodeConnectFrame, runCursorAgent } from "./client.ts";
import type { CursorProto, ProtoClass, ProtoMessage } from "./proto-loader.ts";
import { parseSseStream } from "../../sse.ts";

const originalToken = process.env.CCP_CURSOR_AUTH_TOKEN;

afterEach(() => {
  if (originalToken === undefined) delete process.env.CCP_CURSOR_AUTH_TOKEN;
  else process.env.CCP_CURSOR_AUTH_TOKEN = originalToken;
});

describe("Cursor provider auth errors", () => {
  it("surfaces expired auth before calling Cursor", async () => {
    process.env.CCP_CURSOR_AUTH_TOKEN = jwt({ exp: 1 });

    const response = await cursorProvider.handleMessages(
      {
        model: "cursor",
        messages: [{ role: "user", content: "hello" }],
      },
      fakeCtx(),
    );
    const body = (await response.json()) as {
      error: { type: string; message: string };
    };

    expect(response.status).toBe(401);
    expect(response.headers.get("content-type")).toBe("application/json");
    expect(body.error.type).toBe("authentication_error");
    expect(body.error.message).toContain("expired or near expiry");
  });
});

describe("Cursor provider messages", () => {
  it("returns assistant text for non-streaming requests", async () => {
    const provider = createCursorProvider({
      loadAuth: async () => ({ accessToken: "token", source: "test" }),
      runAgent: async () =>
        streamFromChunks([
          frame({ interactionUpdate: { textDelta: { text: "hello" } } }),
          frame({ interactionUpdate: { turnEnded: { inputTokens: "4", outputTokens: "1" } } }),
          encodeConnectFrame(jsonBytes({}), 2),
        ]),
      proto: fakeProto,
    });

    const response = await provider.handleMessages(
      {
        model: "cursor",
        messages: [{ role: "user", content: "hello" }],
      },
      fakeCtx(),
    );
    const body = (await response.json()) as { content: Array<{ type: string; text?: string }> };

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/json");
    expect(body.content).toEqual([{ type: "text", text: "hello" }]);
  });

  it("returns valid Anthropic SSE for streaming requests", async () => {
    const provider = createCursorProvider({
      loadAuth: async () => ({ accessToken: "token", source: "test" }),
      runAgent: async () =>
        streamFromChunks([
          frame({ interactionUpdate: { textDelta: { text: "streamed" } } }),
          frame({ interactionUpdate: { turnEnded: { inputTokens: "4", outputTokens: "2" } } }),
          encodeConnectFrame(jsonBytes({}), 2),
        ]),
      proto: fakeProto,
    });

    const response = await provider.handleMessages(
      {
        model: "cursor",
        stream: true,
        messages: [{ role: "user", content: "hello" }],
      },
      fakeCtx(),
    );
    const events = [];
    for await (const event of parseSseStream(response.body!)) {
      events.push({ event: event.event, data: JSON.parse(event.data) });
    }

    expect(response.headers.get("content-type")).toBe("text/event-stream");
    expect(response.headers.get("cache-control")).toBe("no-cache");
    expect(response.headers.get("connection")).toBe("keep-alive");
    expect(events.map((event) => event.event)).toContain("message_start");
    expect(events.find((event) => event.event === "content_block_delta")?.data.delta.text).toBe(
      "streamed",
    );
    expect(events.at(-1)?.event).toBe("message_stop");
  });

  it("preserves Cursor Connect end error status for non-streaming requests", async () => {
    const provider = createCursorProvider({
      loadAuth: async () => ({ accessToken: "token", source: "test" }),
      runAgent: async () =>
        streamFromChunks([
          encodeConnectFrame(
            jsonBytes({
              error: {
                code: "resource_exhausted",
                message: "Error",
                details: [
                  {
                    debug: {
                      details: {
                        additionalInfo: {
                          chatMessage: "You've hit your free requests limit.",
                        },
                      },
                    },
                  },
                ],
              },
            }),
            2,
          ),
        ]),
      proto: fakeProto,
    });

    const response = await provider.handleMessages(
      {
        model: "cursor",
        messages: [{ role: "user", content: "hello" }],
      },
      fakeCtx(),
    );
    const body = (await response.json()) as { error: { message: string } };

    expect(response.status).toBe(429);
    expect(body.error.message).toContain("resource_exhausted");
    expect(body.error.message).toContain("free requests limit");
  });

  it("bridges Cursor shellStreamArgs through Claude Bash tool_use and resumes from tool_result", async () => {
    let serverController!: ReadableStreamDefaultController<Uint8Array>;
    const sentFrames: Array<Record<string, any>> = [];
    let finalResponseSent = false;
    const workingDirectory = "/tmp/cursor bridge cwd";
    const serverReadable = new ReadableStream<Uint8Array>({
      start(controller) {
        serverController = controller;
        queueMicrotask(() => {
          controller.enqueue(frame({
            message: {
              case: "execServerMessage",
              value: {
                id: 11,
                execId: "exec-shell",
                message: {
                  case: "shellStreamArgs",
                  value: {
                    command: "printf should-not-run",
                    workingDirectory,
                    timeout: 5000,
                  },
                },
              },
            },
          }));
        });
      },
    });
    const provider = createCursorProvider({
      loadAuth: async () => ({ accessToken: "token", source: "test" }),
      runAgent: async (opts) =>
        runCursorAgent({
          ...opts,
          proto: fakeProto,
          openRunStream: async () => ({
            readable: serverReadable,
            status: Promise.resolve({ status: 200 }),
            async write(frameBytes) {
              const message = decodeFrameJson(frameBytes) as Record<string, any>;
              sentFrames.push(message);
              if (message.execClientMessage?.shellStream?.exit && !finalResponseSent) {
                finalResponseSent = true;
                serverController.enqueue(frame({
                  interactionUpdate: { textDelta: { text: "resumed after native shell" } },
                }));
                serverController.enqueue(frame({
                  interactionUpdate: { turnEnded: { inputTokens: "4", outputTokens: "3" } },
                }));
                serverController.close();
              }
            },
            close() {},
          }),
        }),
      proto: fakeProto,
    });

    const initial = await provider.handleMessages(
      {
        model: "cursor",
        stream: true,
        tools: [{ name: "Bash", input_schema: { type: "object" } }],
        messages: [{ role: "user", content: "run shell" }],
      },
      fakeCtx(),
    );
    const initialEvents = await collectSse(initial);
    const toolStart = initialEvents.find((event) => event.event === "content_block_start"
      && event.data.content_block?.type === "tool_use");

    expect(toolStart?.data.content_block.name).toBe("Bash");
    expect(toolStart?.data.content_block.id).toStartWith("call_cursor_");
    const toolInputDelta = initialEvents.find((event) => event.event === "content_block_delta"
      && event.data.delta?.type === "input_json_delta");
    expect(JSON.parse(toolInputDelta?.data.delta.partial_json).command).toBe(
      "cd '/tmp/cursor bridge cwd' && printf should-not-run",
    );
    expect(initialEvents.find((event) => event.event === "message_delta")?.data.delta.stop_reason).toBe("tool_use");
    expect(sentFrames.some((message) => message.execClientMessage?.shellStream?.stdout)).toBe(false);

    const resume = await provider.handleMessages(
      {
        model: "cursor",
        stream: true,
        messages: [
          {
            role: "assistant",
            content: [{
              type: "tool_use",
              id: toolStart!.data.content_block.id,
              name: "Bash",
              input: { command: "printf should-not-run" },
            }],
          },
          {
            role: "user",
            content: [{
              type: "tool_result",
              tool_use_id: toolStart!.data.content_block.id,
              content: "native shell output",
              is_error: false,
            }],
          },
        ],
      },
      fakeCtx(),
    );
    const resumeEvents = await collectSse(resume);

    expect(sentFrames.some((message) =>
      message.execClientMessage?.shellStream?.stdout?.data === "native shell output"
    )).toBe(true);
    expect(sentFrames.some((message) =>
      message.execClientMessage?.shellStream?.stdout?.data === "should-not-run"
    )).toBe(false);
    expect(resumeEvents.find((event) => event.event === "content_block_delta")?.data.delta.text).toBe(
      "resumed after native shell",
    );
    expect(resumeEvents.find((event) => event.event === "message_delta")?.data.delta.stop_reason).toBe("end_turn");
    expect(resumeEvents.at(-1)?.event).toBe("message_stop");
  });
});

function fakeCtx(): RequestContext {
  return {
    reqId: "req",
    sessionId: "session",
    signal: new AbortController().signal,
    childLogger: () => ({
      debug() {},
      info() {},
      warn() {},
      error() {},
      child() {
        return this;
      },
    }),
  };
}

async function collectSse(response: Response): Promise<Array<{ event: string; data: any }>> {
  const events = [];
  for await (const event of parseSseStream(response.body!)) {
    events.push({ event: event.event ?? "message", data: JSON.parse(event.data) });
  }
  return events;
}

function jwt(payload: Record<string, unknown>): string {
  return [
    Buffer.from(JSON.stringify({ alg: "none", typ: "JWT" })).toString("base64url"),
    Buffer.from(JSON.stringify(payload)).toString("base64url"),
    "signature",
  ].join(".");
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const fakeProto: CursorProto = {
  AgentServerMessage: jsonProtoClass(),
  AgentClientMessage: jsonProtoClass(),
};

function jsonProtoClass(): ProtoClass {
  return {
    fromBinary(bytes: Uint8Array): ProtoMessage {
      return messageFromJson(JSON.parse(decoder.decode(bytes)));
    },
    fromJson(json: unknown): ProtoMessage {
      return messageFromJson(json);
    },
  };
}

function messageFromJson(json: unknown): ProtoMessage {
  return Object.assign(
    {
      toBinary(): Uint8Array {
        return jsonBytes(json);
      },
      toJson(): unknown {
        return json;
      },
    },
    json && typeof json === "object" && !Array.isArray(json) ? json : {},
  );
}

function frame(json: unknown): Uint8Array {
  return encodeConnectFrame(jsonBytes(json));
}

function jsonBytes(json: unknown): Uint8Array {
  return encoder.encode(JSON.stringify(json));
}

function decodeFrameJson(frame: Uint8Array): unknown {
  const len = Buffer.from(frame).readUInt32BE(1);
  return JSON.parse(decoder.decode(frame.slice(5, 5 + len)));
}

function streamFromChunks(chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      controller.close();
    },
  });
}
