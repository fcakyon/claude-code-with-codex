import { afterEach, describe, expect, it } from "bun:test";
import {
  CodexHeaderTimeoutError,
  CodexTransportError,
  isRetryableCodexTransportError,
  setCodexHeaderTimeoutForTests,
} from "./client.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
  setCodexHeaderTimeoutForTests(60_000, 1);
});

describe("CodexHeaderTimeoutError", () => {
  it("identifies header wait timeouts", () => {
    const err = new CodexHeaderTimeoutError(123);

    expect(err.name).toBe("CodexHeaderTimeoutError");
    expect(err.message).toContain("123ms");
  });
});

describe("CodexTransportError", () => {
  it("classifies Bun socket-close fetch failures as retryable transport errors", () => {
    const err = new TypeError(
      "The socket connection was closed unexpectedly. For more information, pass `verbose: true` in the second argument to fetch()",
    );

    expect(isRetryableCodexTransportError(err)).toBe(true);
    expect(new CodexTransportError(err).message).toContain(
      "socket connection was closed unexpectedly",
    );
  });

  it("classifies Codex API operation timeouts as retryable transport errors", () => {
    const err = new TypeError("API Error: The operation timed out.");

    expect(isRetryableCodexTransportError(err)).toBe(true);
    expect(new CodexTransportError(err).message).toContain("operation timed out");
  });

  it("does not retry abort errors as transport failures", () => {
    expect(isRetryableCodexTransportError(new DOMException("Aborted", "AbortError"))).toBe(false);
  });
});
