import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { stateDir } from "../../../paths.ts";
import { encodeSseEvent } from "../../../sse.ts";
import type { Logger } from "../../../log.ts";
import {
  createUpstreamStreamDiagnostics,
  mapUsageToAnthropic,
  reduceUpstream,
  UpstreamStreamError,
} from "./reducer.ts";

/**
 * Translate a Codex Responses SSE stream into Anthropic SSE events.
 * Returns a ReadableStream<Uint8Array> ready to pipe to the client.
 *
 * The HTTP status has already been flushed (200) before the first
 * upstream event is consumed, so rate-limit and upstream-failed cases
 * surface as SSE error events rather than non-200 statuses.
 */
const KEEPALIVE_INTERVAL_MS = 15_000;

export function translateStream(
  upstream: ReadableStream<Uint8Array>,
  opts: {
    messageId: string;
    model: string;
    log: Logger;
    reqId?: string;
    signal?: AbortSignal;
    upstreamHeaders?: Headers;
    onFinish?: (finish: {
      stopReason: "end_turn" | "tool_use" | "max_tokens";
      usage?: Parameters<typeof mapUsageToAnthropic>[0];
    }) => void;
  },
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    async start(controller) {
      const activeTools = new Map<number, { id: string; name: string }>();
      const diagnostics = createUpstreamStreamDiagnostics();
      let closed = false;
      let messageStarted = false;
      let lastEmitAt = 0;
      const safeClose = () => {
        if (closed) return;
        closed = true;
        try {
          controller.close();
        } catch {
          // The stream can be closed by cancellation before the producer observes it.
        }
      };
      const emit = (event: string, data: unknown) => {
        if (closed || opts.signal?.aborted || controller.desiredSize === null) return false;
        try {
          controller.enqueue(encoder.encode(encodeSseEvent(event, data)));
          lastEmitAt = Date.now();
          return true;
        } catch (err) {
          if (opts.signal?.aborted || isClosedControllerError(err)) {
            closed = true;
            return false;
          }
          throw err;
        }
      };
      const emitPingIfStale = () => {
        if (!messageStarted || Date.now() - lastEmitAt < KEEPALIVE_INTERVAL_MS) return;
        emit("ping", { type: "ping" });
      };
      const ensureMessageStart = () => {
        if (messageStarted) return;
        messageStarted = true;
        emit("message_start", {
          type: "message_start",
          message: {
            id: opts.messageId,
            type: "message",
            role: "assistant",
            model: opts.model,
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

      try {
        for await (const e of reduceUpstream(upstream, opts.log, diagnostics)) {
          switch (e.kind) {
            case "text-start":
              ensureMessageStart();
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: { type: "text", text: "" },
              });
              break;
            case "text-delta":
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: { type: "text_delta", text: e.text },
              });
              break;
            case "text-stop":
              emit("content_block_stop", { type: "content_block_stop", index: e.index });
              break;
            case "tool-start":
              activeTools.set(e.index, { id: e.id, name: e.name });
              ensureMessageStart();
              emit("content_block_start", {
                type: "content_block_start",
                index: e.index,
                content_block: {
                  type: "tool_use",
                  id: e.id,
                  name: e.name,
                  input: {},
                },
              });
              break;
            case "tool-delta":
              emit("content_block_delta", {
                type: "content_block_delta",
                index: e.index,
                delta: { type: "input_json_delta", partial_json: e.partialJson },
              });
              break;
            case "tool-progress":
              emitPingIfStale();
              break;
            case "tool-stop":
              activeTools.delete(e.index);
              emit("content_block_stop", { type: "content_block_stop", index: e.index });
              break;
            case "finish":
              ensureMessageStart();
              opts.onFinish?.({ stopReason: e.stopReason, usage: e.usage });
              emit("message_delta", {
                type: "message_delta",
                delta: { stop_reason: e.stopReason, stop_sequence: null },
                usage: mapUsageToAnthropic(e.usage),
              });
              emit("message_stop", { type: "message_stop" });
              break;
          }
        }
      } catch (err) {
        const activeToolNames = Array.from(activeTools.values(), (tool) => tool.name);
        const activeToolCalls = Array.from(activeTools.values());
        if (opts.signal?.aborted) {
          opts.log.info("stream cancelled", {
            err: describeError(err),
            activeToolNames,
            activeToolCalls,
            diagnostics: describeDiagnostics(diagnostics),
          });
          return;
        }
        if (err instanceof UpstreamStreamError) {
          const detail = {
            kind: err.kind,
            message: err.message,
            activeToolNames,
            activeToolCalls,
            clientAborted: opts.signal?.aborted ?? false,
            diagnostics: describeDiagnostics(diagnostics),
            upstreamHeaders: describeHeaders(opts.upstreamHeaders),
          };
          const diagnosticFile = await writeDiagnosticFile(opts.reqId, "upstream-stream-error", detail);
          opts.log.warn("upstream stream error", { ...detail, diagnosticFile });
          ensureMessageStart();
          emit("error", {
            type: "error",
            error: {
              type: err.kind === "rate_limit" ? "rate_limit_error" : "api_error",
              message: err.message,
            },
          });
        } else {
          const detail = {
            err: describeError(err),
            activeToolNames,
            activeToolCalls,
            clientAborted: opts.signal?.aborted ?? false,
            diagnostics: describeDiagnostics(diagnostics),
            upstreamHeaders: describeHeaders(opts.upstreamHeaders),
          };
          const diagnosticFile = await writeDiagnosticFile(opts.reqId, "stream-translation-error", detail);
          opts.log.error("stream translation error", { ...detail, diagnosticFile });
          ensureMessageStart();
          emit("error", {
            type: "error",
            error: { type: "api_error", message: String(err) },
          });
        }
      } finally {
        safeClose();
      }
    },
  });
}

function isClosedControllerError(err: unknown): boolean {
  return err instanceof TypeError && err.message.includes("Controller is already closed");
}

function describeDiagnostics(diagnostics: ReturnType<typeof createUpstreamStreamDiagnostics>) {
  const now = Date.now();
  return {
    bytesRead: diagnostics.stats.bytesRead,
    chunkCount: diagnostics.stats.chunkCount,
    eventCount: diagnostics.stats.eventCount,
    durationMs: now - diagnostics.stats.startedAt,
    msSinceLastChunk: diagnostics.stats.lastChunkAt ? now - diagnostics.stats.lastChunkAt : undefined,
    msSinceLastEvent: diagnostics.stats.lastEventAt ? now - diagnostics.stats.lastEventAt : undefined,
    lastEventType: diagnostics.lastEventType,
    sawTerminalEvent: diagnostics.sawTerminalEvent,
  };
}

function describeHeaders(headers: Headers | undefined): Record<string, string> | undefined {
  if (!headers) return undefined;
  const out: Record<string, string> = {};
  for (const [key, value] of headers) {
    out[key] = value;
  }
  return out;
}

async function writeDiagnosticFile(
  reqId: string | undefined,
  kind: string,
  detail: Record<string, unknown>,
): Promise<string | undefined> {
  try {
    const dir = join(stateDir(), "diagnostics");
    await mkdir(dir, { recursive: true });
    const safeReqId = reqId?.replace(/[^a-zA-Z0-9._-]/g, "_") || "unknown";
    const file = join(dir, `${new Date().toISOString().replace(/[:.]/g, "-")}-${safeReqId}-${kind}.json`);
    await writeFile(file, JSON.stringify({ t: new Date().toISOString(), kind, reqId, ...detail }, null, 2));
    return file;
  } catch {
    return undefined;
  }
}

function describeError(err: unknown) {
  if (!(err instanceof Error)) return { message: String(err) };
  const detail = err as Error & {
    code?: unknown;
    errno?: unknown;
    cause?: unknown;
  };
  return {
    name: err.name,
    message: err.message,
    code: detail.code,
    errno: detail.errno,
    cause: describeCause(detail.cause),
    stack: err.stack,
  };
}

function describeCause(cause: unknown): unknown {
  if (!(cause instanceof Error)) return cause;
  const detail = cause as Error & { code?: unknown; errno?: unknown };
  return {
    name: cause.name,
    message: cause.message,
    code: detail.code,
    errno: detail.errno,
    stack: cause.stack,
  };
}
