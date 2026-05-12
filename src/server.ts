import { createLogger, logDir, REDACT_KEYS } from "./log.ts"

import type { AnthropicRequest } from "./anthropic/schema.ts"
import type { AliasProvider } from "./config.ts"
import type { Provider, RequestContext } from "./providers/types.ts"
import {
  allSupportedModels,
  ANTHROPIC_STYLE_ALIASES,
  providerForModel,
} from "./providers/registry.ts"

const rootLog = createLogger("server")

export interface ServeOptions {
  port: number
}

interface SessionState {
  seq: number
  affinityProvider?: AliasProvider
  lastSeen: number
}

const SESSION_IDLE_TTL_MS = 24 * 60 * 60 * 1000
const MAX_SESSIONS = 10_000
const sessions = new Map<string, SessionState>()

function currentSession(sessionId: string | undefined, now = Date.now()): SessionState | undefined {
  if (!sessionId) return undefined
  const existing = sessions.get(sessionId)
  if (existing && now - existing.lastSeen > SESSION_IDLE_TTL_MS) sessions.delete(sessionId)
  const state = sessions.get(sessionId) ?? { seq: 0, lastSeen: now }
  state.seq += 1
  state.lastSeen = now
  sessions.set(sessionId, state)
  evictSessions(now)
  return state
}

function evictSessions(now: number): void {
  for (const [sessionId, state] of sessions) {
    if (now - state.lastSeen > SESSION_IDLE_TTL_MS) sessions.delete(sessionId)
  }
  while (sessions.size > MAX_SESSIONS) {
    let oldestSessionId: string | undefined
    let oldestLastSeen = Infinity
    for (const [sessionId, state] of sessions) {
      if (state.lastSeen < oldestLastSeen) {
        oldestSessionId = sessionId
        oldestLastSeen = state.lastSeen
      }
    }
    if (!oldestSessionId) return
    sessions.delete(oldestSessionId)
  }
}

export function startServer(opts: ServeOptions): { stop: () => void; port: number } {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: opts.port,
    idleTimeout: 255,
    async fetch(req) {
      const url = new URL(req.url)
      const start = Date.now()
      const reqId = crypto.randomUUID()
      rootLog.info("request", {
        reqId,
        method: req.method,
        path: url.pathname,
        ...(url.search ? { query: redactedQuery(url) } : {}),
      })
      try {
        const resp = await route(req, url, reqId)
        const ms = Date.now() - start
        rootLog.info("response", { reqId, status: resp.status, ms })
        if (!resp.body) return resp
        return wrapStreamResponse(resp, reqId, start, rootLog)
      } catch (err) {
        if (isAbortError(err)) {
          rootLog.info("client disconnected", { reqId, ms: Date.now() - start })
          return new Response(null, { status: 499 })
        }
        rootLog.error("handler error", { reqId, err: String(err), stack: (err as Error)?.stack })
        return jsonError(500, "internal_error", String(err))
      }
    },
  })
  rootLog.info("server listening", { port: server.port, logDir: logDir() })
  return {
    port: Number(server.port),
    stop: () => server.stop(),
  }
}

async function route(req: Request, url: URL, reqId: string): Promise<Response> {
  if (url.pathname === "/healthz") {
    return new Response(JSON.stringify({ ok: true }), {
      headers: { "content-type": "application/json" },
    })
  }

  if (req.method === "POST" && url.pathname === "/v1/messages/count_tokens") {
    const body = await parseJsonBody(req)
    if (body instanceof Response) return body
    const sessionId = req.headers.get("x-claude-code-session-id") || undefined
    const session = currentSession(sessionId)
    const provider = routeProvider(body, reqId, session?.affinityProvider)
    if (provider instanceof Response) return provider
    updateSessionAffinity(session, body.model, provider.name)
    const ctx = buildCtx(req, reqId, provider.name, sessionId, session)
    ctx.childLogger("server").info("dispatch", { model: body.model })
    return provider.handleCountTokens(body, ctx)
  }

  if (req.method === "POST" && url.pathname === "/v1/messages") {
    const body = await parseJsonBody(req)
    if (body instanceof Response) return body
    const sessionId = req.headers.get("x-claude-code-session-id") || undefined
    const session = currentSession(sessionId)
    const provider = routeProvider(body, reqId, session?.affinityProvider)
    if (provider instanceof Response) return provider
    updateSessionAffinity(session, body.model, provider.name)
    const ctx = buildCtx(req, reqId, provider.name, sessionId, session)
    ctx.childLogger("server").info("dispatch", { model: body.model })
    return provider.handleMessages(body, ctx)
  }

  return jsonError(404, "not_found", `No route for ${req.method} ${url.pathname}`)
}

function buildCtx(
  req: Request,
  reqId: string,
  providerName: string,
  sessionId: string | undefined,
  session: SessionState | undefined,
): RequestContext {
  const sessionSeq = session?.seq
  const bindings = { reqId, sessionId, sessionSeq, provider: providerName }
  return {
    reqId,
    sessionId,
    sessionSeq,
    signal: req.signal,
    childLogger: (service) => createLogger(service, bindings),
  }
}

// Claude Code uses a [1m] suffix convention (e.g. "gpt-5.4[1m]") to
// signal that the model should be treated as having a 1M-token context
// window. Claude Code normalizes this away before sending requests to
// the API, but we strip it here too as defense-in-depth in case a
// future version or a different client includes it.
export function normalizeIncomingModel(model: string): string {
  return model.replace(/\[1m\]$/i, "")
}

function updateSessionAffinity(
  session: SessionState | undefined,
  model: string,
  providerName: string,
): void {
  if (!session || ANTHROPIC_STYLE_ALIASES.has(model)) return
  session.affinityProvider = providerName as AliasProvider
}

function routeProvider(
  body: AnthropicRequest,
  reqId: string,
  sessionAliasProvider?: AliasProvider,
): Provider | Response {
  if (!body.model) {
    return jsonError(
      400,
      "invalid_request_error",
      `Missing "model" in request body. ${knownModelsMessage()}`,
    )
  }
  body.model = normalizeIncomingModel(body.model)
  const provider = providerForModel(body.model, sessionAliasProvider)
  if (!provider) {
    rootLog.warn("unknown model", { reqId, model: body.model })
    return jsonError(
      400,
      "invalid_request_error",
      `Unknown model "${body.model}". ${knownModelsMessage()}`,
    )
  }
  return provider
}

function knownModelsMessage(): string {
  const groups = new Map<string, string[]>()
  for (const { model, provider } of allSupportedModels()) {
    const list = groups.get(provider) ?? []
    list.push(model)
    groups.set(provider, list)
  }
  const parts: string[] = []
  for (const [provider, models] of groups) {
    parts.push(`${provider}: ${models.join(", ")}`)
  }
  return `Supported: ${parts.join("; ")}.`
}

async function parseJsonBody(req: Request): Promise<AnthropicRequest | Response> {
  try {
    return (await req.json()) as AnthropicRequest
  } catch (err) {
    return jsonError(400, "invalid_request_error", `Invalid JSON: ${err}`)
  }
}

function isAbortError(err: unknown): boolean {
  return err instanceof Error && err.name === "AbortError"
}

function wrapStreamResponse(
  resp: Response,
  reqId: string,
  start: number,
  log: ReturnType<typeof createLogger>,
): Response {
  const body = resp.body!
  const reader = body.getReader()
  const stream = new ReadableStream<Uint8Array>({
    async pull(controller) {
      try {
        const { done, value } = await reader.read()
        if (done) {
          log.info("request_completed", { reqId, status: resp.status, ms: Date.now() - start })
          controller.close()
          return
        }
        controller.enqueue(value)
      } catch (err) {
        if (isAbortError(err)) {
          log.info("client disconnected", { reqId, ms: Date.now() - start })
        } else {
          log.error("stream error", { reqId, err: String(err) })
        }
        controller.error(err)
      }
    },
    cancel() {
      reader.cancel().catch(() => {})
    },
  })
  const headers = new Headers(resp.headers)
  headers.delete("content-encoding")
  headers.delete("content-length")
  headers.delete("transfer-encoding")
  return new Response(stream, {
    status: resp.status,
    statusText: resp.statusText,
    headers,
  })
}

function redactedQuery(url: URL): Record<string, string> {
  const out: Record<string, string> = {}
  for (const [k, v] of url.searchParams) {
    out[k] = REDACT_KEYS.has(k.toLowerCase()) ? `[redacted len=${v.length}]` : v
  }
  return out
}

function jsonError(status: number, type: string, message: string): Response {
  return new Response(JSON.stringify({ type: "error", error: { type, message } }), {
    status,
    headers: { "content-type": "application/json" },
  })
}
