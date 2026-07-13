# claude-code-with-codex

Use Claude Code with your **Claude subscription and your ChatGPT (Codex)
subscription at the same time**, and switch between them mid-conversation.

<img src="meta/claude-code-screenshot-2026-07.webp" alt="Claude Code running through the proxy" />

It runs as a tiny local proxy. Claude Code already speaks the Anthropic API, so
the proxy sits in front of it and sends each request to the right place based on
the model name:

- Ask for a **Claude** model and it uses your **Claude subscription** (the login
  Claude Code already has). Nothing is translated and no API key is needed.
- Ask for a **`gpt-5.6-*`** model and it uses your **ChatGPT subscription**
  through the Codex login.

So you can keep Opus on your Claude plan for hard work and run the fast slot on
your ChatGPT plan, in the same session, and flip between them whenever you want.

[Quickstart](#quickstart) · [Switching models](#switching-models) ·
[How it works](#how-it-works) · [Configuration](#configuration) ·
[Other backends](#other-backends) · [Limitations](#limitations)

## What you need

- **Claude Code** installed and signed in with a **Claude Pro or Max** plan.
- A **ChatGPT Plus, Pro, or Team** plan and the **Codex CLI** signed in.
- **Rust** only if you install from crates.io or source. The prebuilt binary needs nothing.

## Quickstart

Copy-paste, top to bottom.

```sh
# 1. Install claude-codex. Pick ONE:

#    a) Prebuilt binary, no Rust needed (macOS and Linux):
curl -fsSL https://raw.githubusercontent.com/fcakyon/claude-code-with-codex/main/scripts/install.sh | bash

#    b) From crates.io, if you have Rust (rustup.rs):
cargo install claude-codex --locked

# 2. Sign in to your ChatGPT plan through the Codex CLI
codex login
claude-codex codex auth status # should print your account and expiry

# 3. Start the proxy and leave it running
claude-codex serve # listens on 127.0.0.1:18765
```

To build from a specific commit instead, use
`cargo install --git https://github.com/fcakyon/claude-code-with-codex --locked`.

Then, in a second terminal, point Claude Code at the proxy and launch it:

```sh
export ANTHROPIC_BASE_URL="http://localhost:18765"

# Leave these unset. Claude Code forwards your Claude subscription login for
# Claude models only when no token is set here. Setting one breaks that route.
unset ANTHROPIC_AUTH_TOKEN ANTHROPIC_API_KEY

# The opus slot runs on your Claude plan, the sonnet slot on your ChatGPT plan.
export ANTHROPIC_DEFAULT_OPUS_MODEL="claude-opus-4-8"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-terra"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1

claude
```

That is it. Inside Claude Code, pick **Opus** to run on your Claude plan and
**Sonnet** to run on your ChatGPT plan.

To make it permanent, put the `export` lines in your `~/.zshrc` or `~/.bashrc`.

## Switching models

- **The `/model` picker.** Choosing Opus uses `ANTHROPIC_DEFAULT_OPUS_MODEL`
  (your Claude plan) and choosing Sonnet uses `ANTHROPIC_DEFAULT_SONNET_MODEL`
  (your ChatGPT plan). Switch as often as you like, even mid-conversation.
- **A specific model for one run.** `claude --model gpt-5.6-terra` or
  `claude --model claude-opus-4-8`.
- **List what is available.** `claude-codex models`.

Reasoning is carried across a switch. When you move a conversation from one plan
to the other, the earlier turn's thinking is kept and shown to the next model as
plain tagged text, so context is not lost.

## How it works

Claude Code sends normal Anthropic API requests to the proxy. The proxy reads
the model name and routes:

- **Claude models** are relayed straight to `api.anthropic.com`, untouched,
  reusing the subscription token Claude Code already sends. The request body is
  forwarded as-is so Anthropic's prompt caching keeps working. The proxy stores
  no Claude credentials.
- **Codex models** are translated to the OpenAI Responses API and sent with the
  ChatGPT login from the Codex CLI's `~/.codex/auth.json`. The proxy refreshes
  that token when needed and writes it back so the Codex CLI keeps working.

An unknown model name returns a clear 400 that lists the ids you can use.

## Configuration

Set through environment variables when launching Claude Code.

| Variable                                   | What it does                                                                        |
| ------------------------------------------ | ----------------------------------------------------------------------------------- |
| `ANTHROPIC_BASE_URL`                       | Point Claude Code at the proxy, e.g. `http://localhost:18765`.                      |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`             | Model id for the Opus slot in the picker. Use a `claude-*` id for your Claude plan. |
| `ANTHROPIC_DEFAULT_SONNET_MODEL`           | Model id for the Sonnet slot. Use `gpt-5.6-terra` for your ChatGPT plan.            |
| `ANTHROPIC_MODEL`                          | Force a single model for the whole session instead of using the picker.             |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Set to `1` to skip Claude Code's non-essential background calls.                    |

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`. Either one overrides
the Claude subscription login and the Claude route returns 401.

The proxy listens on `127.0.0.1:18765` by default. Change it with
`PORT=11435 claude-codex serve`, and match `ANTHROPIC_BASE_URL`.

Background requests Claude Code makes for its small, fast model use a Claude id
by default, so they run on your Claude plan. Set `ANTHROPIC_DEFAULT_HAIKU_MODEL`
to a `gpt-5.6-*` id if you would rather run them on your ChatGPT plan.

## Other backends

The same proxy can also route to **Kimi**, **Grok**, and **Cursor** models, each
with its own login. Run `claude-codex models` to see every id, and
`claude-codex <backend> auth status` to check a login. These backends keep
the behavior of the upstream project this is based on.

## Limitations

- Switching plans in the middle of an active tool call (for example pressing Esc
  during a tool use, then switching and continuing) can fail, because the next
  model cannot verify reasoning that came from the other plan. Starting the next
  step fresh avoids it.

## Credits

Built on [`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy),
which provides the Codex, Kimi, Grok, and Cursor backends. This fork adds using
your Claude subscription as a backend alongside Codex, reasoning that survives a
mid-conversation switch, and reading the Codex login from the Codex CLI.
