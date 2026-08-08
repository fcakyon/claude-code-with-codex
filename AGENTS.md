# Fork practices

- Stay close to upstream and avoid style-only divergence.
- Preserve the `claude-codex` crate, binary, and release identity.
- Keep Claude aliases on Anthropic by default and explicit GPT models on Codex.
- Use Codex CLI authentication. Keep automated tests on toy credentials and mock upstreams.
- Preserve unrelated work and run locked format, clippy, and test checks before release.
- Publish releases to crates.io and tag `vX.Y.Z` for prebuilt GitHub binaries.
