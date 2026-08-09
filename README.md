# claude-code-with-codex

[![crates.io](https://img.shields.io/crates/v/claude-codex.svg)](https://crates.io/crates/claude-codex)
[![CI](https://github.com/fcakyon/claude-code-with-codex/actions/workflows/ci.yml/badge.svg)](https://github.com/fcakyon/claude-code-with-codex/actions/workflows/ci.yml)

Use Claude Code with your **Claude subscription and your ChatGPT (Codex)
subscription at the same time**, and switch between them mid-conversation.

<img src="https://github.com/fcakyon/claude-code-with-codex/releases/download/v0.3.0/claude-codex-demo.gif" alt="Claude Code running through the proxy" />

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

**1. Install `claude-codex`** using a prebuilt binary:

Prebuilt binary, no Rust needed (macOS and Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/fcakyon/claude-code-with-codex/main/scripts/install.sh | bash
```

Or install from crates.io if you have Rust:

```sh
cargo install claude-codex --locked
```

**2. Check your Codex CLI login:**

```sh
claude-codex codex auth status
```

Run `codex login` first if no valid account is found.

**3. Point Claude Code at the router** in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:18765"
  }
}
```

**4. Start the router** and leave it running:

```sh
claude-codex serve
```

**5. Restart Claude Code.**

Claude Code now discovers the models exposed by the router. Switch directly:

```text
/model gpt-5.6-sol[1m]
/model claude-opus-5
```

The `[1m]` suffix enables Claude Code's larger-context mode. The router removes
the suffix before sending the model name to Codex.

## Switching models

- **Inside Claude Code.** Run `/model gpt-5.6-sol[1m]` for Codex or
  `/model claude-opus-5` for Claude.
- **For one new session.** Set `ANTHROPIC_MODEL` when launching Claude Code.
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

Only `ANTHROPIC_BASE_URL` is required in Claude Code's user settings. Restart
Claude Code after changing it so `/model` discovers the router's model list.

| Variable                                   | What it does                                                     |
| ------------------------------------------ | ---------------------------------------------------------------- |
| `ANTHROPIC_BASE_URL`                       | Point Claude Code at the proxy, e.g. `http://localhost:18765`.   |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`             | Optionally remap the Opus alias.                                 |
| `ANTHROPIC_DEFAULT_SONNET_MODEL`           | Optionally remap the Sonnet alias.                               |
| `ANTHROPIC_MODEL`                          | Optionally force one model for the whole session.                |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Set to `1` to skip Claude Code's non-essential background calls. |

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`. Either one overrides
the Claude subscription login and the Claude route returns 401.

The proxy listens on `127.0.0.1:18765` by default. Change it with
`PORT=11435 claude-codex serve`, and match `ANTHROPIC_BASE_URL`.

Alias remapping is optional. For example,
`ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5.6-terra` makes `/model sonnet` use Codex.

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
