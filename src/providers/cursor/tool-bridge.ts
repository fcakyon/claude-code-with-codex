import { encodeSseEvent } from "../../sse.ts";
import type { AnthropicRequest, AnthropicToolResultBlock } from "../../anthropic/schema.ts";
import type { Logger } from "../../log.ts";
import type { RequestContext } from "../types.ts";
import {
  appendCursorShellStreamResult,
  cursorShellStreamArgs,
  decodeCursorStream,
  type CursorAppendMessage,
  type CursorShellStreamExec,
  type CursorStreamEvent,
  type CursorUsage,
} from "./client.ts";
import type { CursorProto } from "./proto-loader.ts";
import { cursorUsageToAnthropic } from "./translate/response.ts";

interface PendingShellTool {
  toolUseId: string;
  exec: CursorShellStreamExec;
  command: string;
  workingDirectory: string;
  timeoutMs: number;
  startedAt: number;
  append: CursorAppendMessage;
  resolve(result: ShellToolResult): void;
  result: Promise<ShellToolResult>;
}

interface ShellToolResult {
  content: string;
  isError: boolean;
}

interface CursorBridgeState {
  sessionId: string;
  messageId: string;
  model: string;
  iterator: AsyncGenerator<CursorStreamEvent>;
  pendingNext?: Promise<IteratorResult<CursorStreamEvent>>;
  pendingTool?: PendingShellTool;
  waiters: Array<(tool: PendingShellTool) => void>;
  log: Logger;
  traffic?: RequestContext["traffic"];
  onSession?: (sessionId: string) => void;
}

const bridgeStates = new Map<string, CursorBridgeState>();

export function canBridgeCursorShellTools(body: AnthropicRequest, ctx: RequestContext): boolean {
  return Boolean(ctx.sessionId && body.stream && body.tools?.some((tool) => tool.name === "Bash"));
}

export function createCursorShellToolBridge(opts: {
  sessionId: string;
  messageId: string;
  model: string;
  log: Logger;
  traffic?: RequestContext["traffic"];
  proto?: CursorProto;
  onSession?: (sessionId: string) => void;
}): {
  shellStreamHandler: (exec: CursorShellStreamExec, append: CursorAppendMessage) => Promise<void>;
  stream: (upstream: ReadableStream<Uint8Array>, signal?: AbortSignal) => ReadableStream<Uint8Array>;
} {
  const state: CursorBridgeState = {
    sessionId: opts.sessionId,
    messageId: opts.messageId,
    model: opts.model,
    iterator: undefined as unknown as AsyncGenerator<CursorStreamEvent>,
    waiters: [],
    log: opts.log,
    traffic: opts.traffic,
    onSession: opts.onSession,
  };

  const notifyTool = (tool: PendingShellTool) => {
    state.pendingTool = tool;
    for (const waiter of state.waiters.splice(0)) waiter(tool);
  };

  return {
    async shellStreamHandler(exec, append) {
      const { command, workingDirectory, timeoutMs } = cursorShellStreamArgs(exec);
      const toolUseId = `call_cursor_${crypto.randomUUID().replace(/-/g, "")}`;
      let resolve!: (result: ShellToolResult) => void;
      const result = new Promise<ShellToolResult>((r) => {
        resolve = r;
      });
      const tool: PendingShellTool = {
        toolUseId,
        exec,
        command,
        workingDirectory,
        timeoutMs,
        startedAt: Date.now(),
        append,
        resolve,
        result,
      };
      opts.traffic?.writeJsonEvent("038-cursor-tool-bridge-pause", {
        toolUseId,
        command,
        workingDirectory,
        timeoutMs,
      });
      notifyTool(tool);
      const shellResult = await result;
      await appendCursorShellStreamResult(
        exec,
        {
          stdout: shellResult.isError ? undefined : shellResult.content,
          stderr: shellResult.isError ? shellResult.content : undefined,
          exitCode: shellResult.isError ? 1 : 0,
          cwd: workingDirectory,
          localExecutionTimeMs: Date.now() - tool.startedAt,
        },
        append,
      );
      opts.traffic?.writeJsonEvent("038-cursor-tool-bridge-resume", {
        toolUseId,
        isError: shellResult.isError,
        contentChars: shellResult.content.length,
      });
    },
    stream(upstream, signal) {
      state.iterator = decodeCursorStream(upstream, opts.proto, {
        traffic: opts.traffic,
        log: opts.log,
      });
      bridgeStates.set(opts.sessionId, state);
      return streamBridgeUntilToolOrEnd(state, signal);
    },
  };
}

export function resumeCursorShellToolBridge(
  body: AnthropicRequest,
  ctx: RequestContext,
  messageId: string,
): Response | undefined {
  const sessionId = ctx.sessionId;
  if (!sessionId) return undefined;
  const state = bridgeStates.get(sessionId);
  const tool = state?.pendingTool;
  if (!state || !tool) return undefined;
  const result = findToolResult(body, tool.toolUseId);
  if (!result) return undefined;

  state.pendingTool = undefined;
  state.messageId = messageId;
  state.model = body.model;
  tool.resolve({
    content: renderToolResultContent(result.content),
    isError: Boolean(result.is_error),
  });

  const stream = streamBridgeUntilToolOrEnd(state, ctx.signal);
  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      "connection": "keep-alive",
    },
  });
}

function streamBridgeUntilToolOrEnd(
  state: CursorBridgeState,
  signal?: AbortSignal,
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      let closed = false;
      let started = false;
      let thinkingOpen = false;
      let textOpen = false;
      let nextIndex = 0;
      let thinkingIndex = -1;
      let textIndex = -1;
      let finalUsage: CursorUsage | undefined;

      const emit = (event: string, data: unknown) => {
        if (closed || signal?.aborted || controller.desiredSize === null) return false;
        state.traffic?.writeJsonEvent("050-downstream-event", { event, data });
        controller.enqueue(encoder.encode(encodeSseEvent(event, data)));
        return true;
      };

      const ensureStart = () => {
        if (started) return;
        started = true;
        emit("message_start", {
          type: "message_start",
          message: {
            id: state.messageId,
            type: "message",
            role: "assistant",
            model: state.model,
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: {
              input_tokens: 0,
              output_tokens: 0,
              cache_creation_input_tokens: 0,
              cache_read_input_tokens: 0,
            },
          },
        });
        emit("ping", { type: "ping" });
      };

      const openThinking = () => {
        if (thinkingOpen) return;
        ensureStart();
        thinkingOpen = true;
        thinkingIndex = nextIndex++;
        emit("content_block_start", {
          type: "content_block_start",
          index: thinkingIndex,
          content_block: { type: "thinking", thinking: "", signature: "" },
        });
      };

      const openText = () => {
        if (textOpen) return;
        ensureStart();
        textOpen = true;
        textIndex = nextIndex++;
        emit("content_block_start", {
          type: "content_block_start",
          index: textIndex,
          content_block: { type: "text", text: "" },
        });
      };

      const closeOpenBlocks = () => {
        if (thinkingOpen) {
          emit("content_block_stop", { type: "content_block_stop", index: thinkingIndex });
          thinkingOpen = false;
        }
        if (textOpen) {
          emit("content_block_stop", { type: "content_block_stop", index: textIndex });
          textOpen = false;
        }
      };

      const emitToolUseAndPause = (tool: PendingShellTool) => {
        closeOpenBlocks();
        ensureStart();
        const index = nextIndex++;
        const input = JSON.stringify({
          command: claudeBashCommand(tool),
          timeout: tool.timeoutMs,
          description: "Run Cursor-requested shell command",
          run_in_background: false,
          dangerouslyDisableSandbox: false,
        });
        emit("content_block_start", {
          type: "content_block_start",
          index,
          content_block: {
            type: "tool_use",
            id: tool.toolUseId,
            name: "Bash",
            input: {},
          },
        });
        emit("content_block_delta", {
          type: "content_block_delta",
          index,
          delta: { type: "input_json_delta", partial_json: input },
        });
        emit("content_block_stop", { type: "content_block_stop", index });
        emit("message_delta", {
          type: "message_delta",
          delta: { stop_reason: "tool_use", stop_sequence: null },
          usage: cursorUsageToAnthropic(finalUsage),
        });
        emit("message_stop", { type: "message_stop" });
      };

      try {
        while (!signal?.aborted) {
          const next = state.pendingNext ?? state.iterator.next();
          state.pendingNext = next;
          const result = await Promise.race([
            next.then((value) => ({ type: "event" as const, value })),
            waitForPendingTool(state).then((tool) => ({ type: "tool" as const, tool })),
          ]);

          if (result.type === "tool") {
            emitToolUseAndPause(result.tool);
            return;
          }

          state.pendingNext = undefined;
          if (result.value.done) break;
          const event = result.value.value;
          state.traffic?.writeJsonEvent("040-cursor-event", event);
          switch (event.type) {
            case "session":
              state.onSession?.(event.sessionId);
              break;
            case "thinking_delta":
              openThinking();
              emit("content_block_delta", {
                type: "content_block_delta",
                index: thinkingIndex,
                delta: { type: "thinking_delta", thinking: event.text },
              });
              break;
            case "text_delta":
              if (thinkingOpen) {
                emit("content_block_stop", { type: "content_block_stop", index: thinkingIndex });
                thinkingOpen = false;
              }
              openText();
              emit("content_block_delta", {
                type: "content_block_delta",
                index: textIndex,
                delta: { type: "text_delta", text: event.text },
              });
              break;
            case "usage":
              finalUsage = event.usage;
              break;
            case "end":
              break;
          }
        }

        ensureStart();
        closeOpenBlocks();
        emit("message_delta", {
          type: "message_delta",
          delta: { stop_reason: "end_turn", stop_sequence: null },
          usage: cursorUsageToAnthropic(finalUsage),
        });
        emit("message_stop", { type: "message_stop" });
        bridgeStates.delete(state.sessionId);
      } catch (err) {
        state.log.warn("cursor bridge stream error", { err: String(err) });
        ensureStart();
        closeOpenBlocks();
        emit("error", {
          type: "error",
          error: { type: "api_error", message: String(err) },
        });
        bridgeStates.delete(state.sessionId);
      } finally {
        closed = true;
        try {
          controller.close();
        } catch {}
      }
    },
  });
}

function waitForPendingTool(state: CursorBridgeState): Promise<PendingShellTool> {
  if (state.pendingTool) return Promise.resolve(state.pendingTool);
  return new Promise((resolve) => state.waiters.push(resolve));
}

function findToolResult(body: AnthropicRequest, toolUseId: string): AnthropicToolResultBlock | undefined {
  for (let i = body.messages.length - 1; i >= 0; i--) {
    const message = body.messages[i];
    if (!message || message.role !== "user" || typeof message.content === "string") continue;
    for (const block of message.content) {
      if (block.type === "tool_result" && block.tool_use_id === toolUseId) return block;
    }
  }
  return undefined;
}

function renderToolResultContent(content: AnthropicToolResultBlock["content"]): string {
  if (typeof content === "string") return content;
  return content
    .map((block) => {
      if (block.type === "text") return block.text;
      if (block.type === "image") return "[image result omitted]";
      if (block.type === "thinking") return block.thinking;
      return JSON.stringify(block);
    })
    .join("\n");
}

function claudeBashCommand(tool: PendingShellTool): string {
  if (!tool.workingDirectory || tool.workingDirectory === process.cwd()) return tool.command;
  return `cd ${shellQuote(tool.workingDirectory)} && ${tool.command}`;
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}
