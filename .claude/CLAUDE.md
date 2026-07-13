# CLAUDE.md

Guidance for Claude Code and other AI tools working on this repository.

## What this is

A small local proxy that lets Claude Code talk to more than one backend at once,
chosen per request by the model name. Claude Code already speaks the Anthropic
Messages API, so the proxy speaks it too and forwards or translates each request:

- `claude-*` models go to Anthropic as a transparent passthrough that reuses
  Claude Code's own subscription login. No API key, no translation.
- `gpt-5.6-*` (and the other codex ids) go to the Codex backend using the
  ChatGPT subscription that the Codex CLI already logged in.
- `kimi-*`, `grok-*`, and `cursor*` ids go to their own translators.

The headline use is running the opus slot on a Claude subscription and the
sonnet slot on a ChatGPT/Codex subscription in the same session, switching
freely mid-conversation.

This is a fork of `raine/claude-code-proxy`. The dual Claude plus Codex routing,
the reasoning-across-switch handling, and the Codex-CLI credential model are the
changes made on top of it.

## Architecture

- `src/registry.rs` picks a provider for each request. `claude-*` and the opus
  and sonnet aliases resolve to the Anthropic passthrough; other ids match a
  backend exactly; an unknown id returns a 400 that lists the supported ids.
- `src/providers/anthropic/mod.rs` is the passthrough. It relays the original
  body and headers to `api.anthropic.com` and streams the reply back. It holds
  no Anthropic credentials.
- `src/providers/codex/*` translates the Anthropic Messages API to the OpenAI
  Responses API and back.
- `src/providers/translate_shared.rs` holds types and helpers shared by the
  translators, including the reasoning-tag helpers.

## Invariants to preserve

- The Anthropic passthrough must stay byte-exact for normal traffic. The only
  rewrite it performs is converting a signature-less `thinking` block into a
  tagged `text` block, and it reserializes only when such a block is present.
  Anything that reserializes every request would evict Anthropic's prompt cache.
- Reasoning stays portable across a model switch. A `thinking` block written by
  one backend cannot be replayed to the other in native form (Anthropic rejects
  a signature-less `thinking` block; the Responses API has no `thinking`
  container). Both translators convert it to text wrapped in the shared
  `REASONING_OPEN` and `REASONING_CLOSE` tags via `wrap_reasoning`. Keep this
  deterministic so the rewritten prefix is byte-stable turn to turn.
- Codex credentials come only from the Codex CLI's `~/.codex/auth.json`. The
  proxy has no Codex login of its own. Token refresh writes back to that file so
  the Codex CLI keeps working (OpenAI rotates the refresh token on use).

## Naming and distribution

The crate, the installed command, and the library target are all `claude-codex`
(`claude_codex` for the library, derived automatically from the package name).
The crates.io package is `claude-codex`. The GitHub repository stays
`claude-code-with-codex`.

Some strings deliberately keep the old `claude-code-proxy` name because they are
compatibility contracts, not the user-facing name. Do not rename them in a
future cleanup:

- The on-disk config and data directory and the macOS Keychain service, in
  `paths.rs`, `providers/kimi/auth`, and `providers/cursor/auth.rs`. Renaming
  these orphans any saved kimi, grok, or cursor login. `paths.rs` already has
  `legacy_config_dir` as the migration hook if this is ever changed on purpose.
- The Codex `ORIGINATOR` and `User-Agent` in `providers/codex`. These go to the
  ChatGPT backend, so keep them stable to avoid changing what the server sees.

Two install paths ship. crates.io via `cargo install claude-codex`, and prebuilt
binaries from the `v*`-tag release workflow in `.github/workflows/release.yml`.
That workflow uses the default `GITHUB_TOKEN` and needs no secrets or Homebrew
tap.

## Build and test

- `cargo build`
- `cargo test -- --test-threads=1`. Run tests single-threaded. A few config
  tests mutate process-wide environment variables and race under the default
  parallel runner. This is pre-existing and unrelated to product behavior.
- `just check` runs format, clippy, and tests together where the toolchain has
  clippy installed.

## Gotchas

- In the dual setup, `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` must be
  unset. Claude Code forwards its subscription login for `claude-*` only when no
  explicit token is present. Setting either one sends that value instead and the
  Anthropic route returns 401.
- Claude Code's web search is a client-side `WebSearch` function tool. The
  hosted `web_search_20250305` tool only appears inside an isolated, history-free
  inner call that Claude Code makes to run the search, so its reconstructed
  `server_tool_use` and `web_search_tool_result` blocks never enter the outer
  transcript. The passthrough logs `hosted_web_search_in_history` if that ever
  changes, which would mean this assumption needs rechecking.

## Known limitation

Switching backends in the middle of an active tool call (for example pressing
Esc during a codex tool use, then switching to a Claude model and continuing)
can fail. Anthropic requires a leading signed `thinking` block in that position
and no valid signature can be produced for reasoning that came from another
backend. This is rare and not worked around.

## Style

Match the surrounding code. `reqwest` is built without gzip or brotli so bodies
are never auto-decompressed, and with rustls so no OS keychain is touched. Keep
new code in the same shape as the module it lives in.
