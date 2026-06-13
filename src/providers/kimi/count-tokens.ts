import { encode } from "gpt-tokenizer/model/gpt-4o";
import type { AnthropicRequest } from "../../anthropic/schema.ts";
import type { KimiChatRequest } from "./translate/request.ts";
import { flattenSystemText } from "../translate/anthropic-content.ts";
import { countAnthropicRequestTokens } from "../shared/count-tokens.ts";
import { countToolSchemaTokens } from "../shared/tool-schema.ts";

const IMAGE_TOKEN_ESTIMATE = 2000;

// Approximate: Kimi's tokenizer isn't gpt-tokenizer, but Claude Code's
// compaction logic only needs a monotonic estimate, not an exact count.
export function countTokens(req: AnthropicRequest): number {
  let total = 0;
  const system = flattenSystemText(req.system);
  if (system) total += encode(system).length;
  total += countAnthropicRequestTokens({
    req,
    countToken: (value) => encode(value).length,
    tools: req.tools,
    readToolName: (tool) => tool.name,
    readToolDescription: (tool) => tool.description,
    readToolSchema: (tool) => tool.input_schema,
    includeThinking: true,
  });
  return total;
}

export function countTranslatedTokens(req: KimiChatRequest): number {
  let total = 0;
  for (const m of req.messages) {
    if (m.role === "system") {
      total += encode(m.content).length;
    } else if (m.role === "user") {
      if (typeof m.content === "string") total += encode(m.content).length;
      else {
        for (const p of m.content) {
          if (p.type === "text") total += encode(p.text).length;
          else total += IMAGE_TOKEN_ESTIMATE;
        }
      }
    } else if (m.role === "assistant") {
      if (typeof m.content === "string") total += encode(m.content).length;
      if (m.reasoning_content) total += encode(m.reasoning_content).length;
      for (const tc of m.tool_calls ?? []) {
        total += encode(tc.function.name).length;
        total += encode(tc.function.arguments).length;
      }
    } else if (m.role === "tool") {
      if (typeof m.content === "string") total += encode(m.content).length;
      else {
        for (const p of m.content) {
          if (p.type === "text") total += encode(p.text).length;
          else total += IMAGE_TOKEN_ESTIMATE;
        }
      }
    }
  }

  total += countToolSchemaTokens(
    req.tools,
    (tool) => tool.function.name,
    (tool) => tool.function.description,
    (tool) => tool.function.parameters,
  );

  total += req.messages.length * 4;
  return total;
}
