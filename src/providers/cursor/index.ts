import type { AnthropicRequest } from "../../anthropic/schema.ts";
import { wantsDownstreamStream } from "../../anthropic/stream.ts";
import { jsonError, jsonResponse, sseResponse } from "../../anthropic/response.ts";
import { logVerbose } from "../../config.ts";
import type { CliHandlers, Provider, RequestContext } from "../types.ts";
import {
  CursorError,
  runCursorAgent,
  type CursorRunOptions,
} from "./client.ts";
import { countCursorTokens } from "./count-tokens.ts";
import {
  cursorAuthLocation,
  expiredAuthMessage,
  loadCursorAuth,
  missingAuthMessage,
  clearCursorAuth,
} from "./auth/token-store.ts";
import { runCursorLogin } from "./auth/login.ts";
import { renderCursorPrompt } from "./translate/request.ts";
import { CURSOR_SUPPORTED_MODELS, resolveCursorModel } from "./translate/model.ts";
import {
  accumulateCursorResponse,
  translateCursorStream,
} from "./translate/response.ts";
import {
  cursorConversationForRequest,
  recordCursorConversation,
} from "./session.ts";
import {
  canBridgeCursorShellTools,
  createCursorShellToolBridge,
  resumeCursorShellToolBridge,
} from "./tool-bridge.ts";
import type { CursorAuth } from "./auth/token-store.ts";
import type { CursorProto } from "./proto-loader.ts";

const AUTH_EXPIRY_SKEW_MS = 60_000;

export interface CursorProviderDeps {
  loadAuth: () => Promise<CursorAuth | undefined>;
  runAgent: (opts: CursorRunOptions) => Promise<ReadableStream<Uint8Array>>;
  proto?: CursorProto;
}

const defaultDeps: CursorProviderDeps = {
  loadAuth: () => loadCursorAuth(),
  runAgent: runCursorAgent,
};

async function handleCountTokens(body: AnthropicRequest, ctx: RequestContext): Promise<Response> {
  const tokens = countCursorTokens(body);
  ctx.childLogger("provider.cursor").debug("count_tokens", { tokens });
  return jsonResponse({ input_tokens: tokens });
}

async function handleMessages(
  body: AnthropicRequest,
  ctx: RequestContext,
  deps: CursorProviderDeps,
): Promise<Response> {
  const log = ctx.childLogger("provider.cursor");
  const messageId = `msg_${crypto.randomUUID().replace(/-/g, "")}`;
  const resumed = resumeCursorShellToolBridge(body, ctx, messageId);
  if (resumed) return resumed;

  const selection = resolveCursorModel(body);
  const prompt = renderCursorPrompt(body);
  const wantStream = wantsDownstreamStream(body);
  const conversationId = cursorConversationForRequest(body, ctx.sessionId);

  log.debug("cursor request", {
    requestedModel: body.model,
    resolvedModel: selection.requestedModel,
    mode: selection.mode,
    conversationId,
    stream: wantStream,
    messageCount: body.messages.length,
    promptChars: prompt.length,
  });
  if (logVerbose()) log.debug("cursor prompt", { prompt });

  const auth = await deps.loadAuth();
  if (!auth) return jsonError(401, "authentication_error", missingAuthMessage());
  if (auth.expires && auth.expires <= Date.now() + AUTH_EXPIRY_SKEW_MS) {
    return jsonError(401, "authentication_error", expiredAuthMessage(auth));
  }

  const onSession = (cursorSessionId: string) => {
    recordCursorConversation(ctx.sessionId, cursorSessionId);
    log.debug("cursor session observed", { cursorSessionId });
  };
  const shellBridge = wantStream && ctx.sessionId && canBridgeCursorShellTools(body, ctx)
    ? createCursorShellToolBridge({
      sessionId: ctx.sessionId,
      messageId,
      model: body.model,
      log: ctx.childLogger("cursor.bridge"),
      traffic: ctx.traffic,
      proto: deps.proto,
      onSession,
    })
    : undefined;

  let upstream: ReadableStream<Uint8Array>;
  try {
    upstream = await deps.runAgent({
      prompt,
      mode: selection.mode,
      conversationId,
      model: selection.requestedModel,
      auth,
      ctx,
      shellStreamHandler: shellBridge?.shellStreamHandler,
    });
  } catch (err) {
    if (err instanceof CursorError) {
      log.warn("cursor upstream error", {
        status: err.status,
        message: err.message,
        detail: err.detail,
      });
      const type = err.status === 401 || err.status === 403 ? "authentication_error" : "api_error";
      return jsonError(err.status, type, err.detail || err.message);
    }
    throw err;
  }

  if (wantStream) {
    const stream = shellBridge?.stream(upstream, ctx.signal) ?? translateCursorStream(upstream, {
      messageId,
      model: body.model,
      log: ctx.childLogger("cursor.stream"),
      signal: ctx.signal,
      traffic: ctx.traffic,
      proto: deps.proto,
      onSession,
    });
    return sseResponse(stream);
  }

  try {
    const result = await accumulateCursorResponse(upstream, {
      messageId,
      model: body.model,
      log: ctx.childLogger("cursor.accumulate"),
      traffic: ctx.traffic,
      proto: deps.proto,
      onSession,
    });
    return jsonResponse(result.response);
  } catch (err) {
    log.warn("cursor accumulate error", { err: String(err) });
    if (err instanceof CursorError) {
      const type = err.status === 401 || err.status === 403 ? "authentication_error" : "api_error";
      return jsonError(err.status, type, err.detail || err.message);
    }
    return jsonError(502, "api_error", String(err));
  }
}

const cli: CliHandlers = {
  async login() {
    const auth = await runCursorLogin();
    if (!auth) {
      console.error("Cursor login did not complete.");
      process.exit(1);
    }
    console.log();
    console.log(`Logged in. Storage: ${auth.source}`);
    if (auth.email) console.log(`Email: ${auth.email}`);
    if (auth.userId) console.log(`User: ${auth.userId}`);
    if (auth.expires) console.log(`Expires: ${new Date(auth.expires).toISOString()}`);
  },
  async status() {
    const auth = await loadCursorAuth();
    if (!auth) {
      console.log("Not authenticated");
      console.log(missingAuthMessage());
      process.exit(1);
    }
    console.log(`Storage: ${auth.source}`);
    if (auth.email) console.log(`Email: ${auth.email}`);
    if (auth.userId) console.log(`User: ${auth.userId}`);
    if (auth.expires) {
      const ms = auth.expires - Date.now();
      console.log(`Expires: ${new Date(auth.expires).toISOString()} (in ${Math.floor(ms / 1000)}s)`);
    } else {
      console.log("Expires: unknown");
    }
  },
  async logout() {
    await clearCursorAuth();
    console.log(`Cleared Cursor auth from ${cursorAuthLocation()}`);
  },
};

export function createCursorProvider(deps: CursorProviderDeps = defaultDeps): Provider {
  return {
    name: "cursor",
    supportedModels: CURSOR_SUPPORTED_MODELS,
    handleMessages: (body, ctx) => handleMessages(body, ctx, deps),
    handleCountTokens,
    cli,
  };
}

export const cursorProvider: Provider = createCursorProvider();
