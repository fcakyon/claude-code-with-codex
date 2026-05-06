import { describe, expect, it } from "bun:test"
import { providerForModel } from "./registry.ts"
import { normalizeIncomingModel } from "../server.ts"

describe("provider routing", () => {
  it("routes fast Codex model aliases after context suffix normalization", () => {
    const model = normalizeIncomingModel("gpt-5.4-fast[1m]")

    expect(model).toBe("gpt-5.4-fast")
    expect(providerForModel(model)?.name).toBe("codex")
  })
})
