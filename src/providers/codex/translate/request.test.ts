import { afterEach, describe, expect, it } from "bun:test"
import { loadConfig } from "../../../config.ts"
import type { AnthropicRequest } from "../../../anthropic/schema.ts"
import { InvalidServiceTierError, translateRequest } from "./request.ts"
const baseRequest: AnthropicRequest = {
  model: "claude-sonnet-4-6",
  messages: [{ role: "user", content: "hello" }],
}

afterEach(() => {
  loadConfig({ forceReload: true })
})

describe("translateRequest", () => {
  it("omits reasoning include when reasoning is not enabled", () => {
    const translated = translateRequest(baseRequest)

    expect(translated.reasoning).toBeUndefined()
    expect(translated.include).toBeUndefined()
  })

  it("includes encrypted reasoning content when reasoning is enabled", () => {
    const translated = translateRequest({
      ...baseRequest,
      output_config: { effort: "medium" },
    })

    expect(translated.reasoning).toEqual({ effort: "medium" })
    expect(translated.include).toEqual(["reasoning.encrypted_content"])
  })

  it("normalizes fast service tier to upstream priority", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "fast" }, forceReload: true })

    const translated = translateRequest(baseRequest)

    expect(translated.service_tier).toBe("priority")
  })

  it("passes flex service tier through", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "flex" }, forceReload: true })

    const translated = translateRequest(baseRequest)

    expect(translated.service_tier).toBe("flex")
  })

  it("uses model service tier when no override is set", () => {
    loadConfig({ env: {}, forceReload: true })

    const translated = translateRequest(baseRequest, { serviceTier: "priority" })

    expect(translated.service_tier).toBe("priority")
  })

  it("service tier override takes precedence over model service tier", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "flex" }, forceReload: true })

    const translated = translateRequest(baseRequest, { serviceTier: "priority" })

    expect(translated.service_tier).toBe("flex")
  })

  it("rejects invalid service tier overrides", () => {
    loadConfig({ env: { CCP_CODEX_SERVICE_TIER: "standard" }, forceReload: true })

    expect(() => translateRequest(baseRequest)).toThrow(InvalidServiceTierError)
    expect(() => translateRequest(baseRequest)).toThrow('Invalid service tier override: "standard"')
  })

  it("returns only the expected top-level upstream request fields", () => {
    const translated = translateRequest({
      ...baseRequest,
      system: "follow instructions",
      tools: [
        {
          name: "lookup_weather",
          description: "Look up the weather",
          input_schema: {
            type: "object",
            properties: { city: { type: "string" } },
            required: ["city"],
          },
        },
      ],
      tool_choice: { type: "tool", name: "lookup_weather" },
      output_config: {
        effort: "high",
        format: {
          type: "json_schema",
          name: "weather_response",
          schema: {
            type: "object",
            properties: { forecast: { type: "string" } },
            required: ["forecast"],
          },
        },
      },
    })

    expect(Object.keys(translated).sort()).toEqual([
      "include",
      "input",
      "instructions",
      "model",
      "parallel_tool_calls",
      "reasoning",
      "store",
      "stream",
      "text",
      "tool_choice",
      "tools",
    ])
  })
})
