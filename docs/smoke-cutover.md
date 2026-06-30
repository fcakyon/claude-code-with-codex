# Smoke cutover

This guide describes the Rust proxy smoke coverage for Kimi, Codex HTTP, and
Codex WebSocket.

## Automated coverage

Run:

```sh
cargo test --test smoke_cutover
just check
```

The smoke tests use local mock upstreams and isolated auth files under a
temporary `CCP_CONFIG_DIR`. They validate `/healthz`, model-based routing,
Kimi chat-completions request shape, Codex HTTP Responses request shape, and
Codex WebSocket Responses request shape.

## Optional real-auth check

Use this only when local accounts and stored credentials are available. Kimi can
create credentials with `claude-code-proxy kimi auth login`. Codex status and
logout read stored credentials; Codex login and device auth are outside the
current Rust support scope, so Codex real-auth smoke uses an existing Codex auth
file.

Check auth first:

```sh
claude-code-proxy kimi auth status
claude-code-proxy codex auth status
```

Run one server at a time in a dedicated terminal:

```sh
CCP_CODEX_TRANSPORT=http claude-code-proxy serve --port 18765
```

Then send a minimal Claude Code turn from another terminal:

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
ANTHROPIC_AUTH_TOKEN=unused \
ANTHROPIC_MODEL=gpt-5.5 \
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.4-mini \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1 \
claude
```

Stop the server and repeat with `CCP_CODEX_TRANSPORT=websocket` or a Kimi model
such as `kimi-for-coding`.

## Known differences

- Kimi upstream streaming uses OpenAI-style chat-completions SSE. The proxy
  emits Anthropic message events and preserves Kimi thinking as Anthropic
  thinking blocks.
- Codex HTTP uses Responses SSE with `stream: true` in the JSON body.
- Codex WebSocket uses the Responses WebSocket protocol and omits `stream`
  from the WebSocket payload.
- Traffic captures include inbound Anthropic requests and routing metadata from
  the server. Provider-specific upstream captures are available where the
  provider writes them. Auth and account headers are redacted.

## Rust support scope

- Implemented: local server, provider routing, Kimi auth/status/logout and
  messages, Codex status/logout and messages, Codex HTTP transport, Codex
  WebSocket transport, local count_tokens, traffic capture, and Cursor provider
  routing placeholders documented elsewhere.
- Codex login and device auth are outside the current Rust support scope.
- Supported without external services in tests: Kimi, Codex HTTP, Codex
  WebSocket, health, routing, and token counting.
- Real-auth validation depends on the local upstream accounts and model
  entitlements.
