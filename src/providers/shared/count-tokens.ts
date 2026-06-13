import { countToolSchemaTokens } from "./tool-schema.ts";
import { normalizeContent, toolResultToString } from "../translate/anthropic-content.ts";
import type { AnthropicRequest } from "../../anthropic/schema.ts";

const IMAGE_TOKEN_ESTIMATE = 2000;

export type AnthropicRequestTokenCounter = (text: string) => number;

type AnthropicToolReaders<TTool> = {
  readToolName: (tool: TTool) => string;
  readToolDescription: (tool: TTool) => string | undefined;
  readToolSchema: (tool: TTool) => unknown;
};

type CountAnthropicRequestTokenOptions<TTool> = {
  req: AnthropicRequest;
  countToken: AnthropicRequestTokenCounter;
  tools?: TTool[];
} & AnthropicToolReaders<TTool>;

export function countAnthropicRequestTokens<TTool>(
  options: CountAnthropicRequestTokenOptions<TTool> & {
    includeThinking?: false;
  },
): number;

export function countAnthropicRequestTokens<TTool>(
  options: CountAnthropicRequestTokenOptions<TTool> & {
    includeThinking: true;
  },
): number;

export function countAnthropicRequestTokens<TTool>(
  {
    req,
    countToken,
    tools,
    readToolName,
    readToolDescription,
    readToolSchema,
    includeThinking = false,
  }: CountAnthropicRequestTokenOptions<TTool> & { includeThinking?: boolean },
): number {
  let total = 0;
  for (const msg of req.messages) {
    const blocks = normalizeContent(msg.content);
    for (const block of blocks) {
      if (block.type === "text") {
        total += countToken(block.text);
      } else if (block.type === "image") {
        total += IMAGE_TOKEN_ESTIMATE;
      } else if (block.type === "tool_use") {
        total += countToken(block.name);
        total += countToken(JSON.stringify(block.input ?? {}));
      } else if (block.type === "tool_result") {
        total += countToken(toolResultToString(block.content));
      } else if (includeThinking && block.type === "thinking") {
        total += countToken(block.thinking);
      }
    }
  }

  total += countToolSchemaTokens(
    tools,
    readToolName,
    readToolDescription,
    readToolSchema,
  );

  total += req.messages.length * 4;
  return total;
}
