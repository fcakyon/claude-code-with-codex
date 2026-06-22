export interface RecoveredCursorToolUse {
  type: "tool_use";
  id: string;
  originalId?: string;
  name: string;
  input: Record<string, unknown>;
}

export type RecoveredCursorTextEvent =
  | { type: "text"; text: string }
  | RecoveredCursorToolUse;

export class CursorToolUseXmlParser {
  private buffer = "";
  private recoveredToolUse = false;
  private readonly allowedToolNames?: ReadonlySet<string>;
  private readonly idFactory: () => string;

  constructor(opts: {
    allowedToolNames?: ReadonlySet<string>;
    idFactory?: () => string;
  } = {}) {
    this.allowedToolNames = opts.allowedToolNames;
    this.idFactory = opts.idFactory ?? newCursorToolUseId;
  }

  get sawToolUse(): boolean {
    return this.recoveredToolUse;
  }

  push(text: string): RecoveredCursorTextEvent[] {
    this.buffer += text;
    return this.drain(false);
  }

  flush(): RecoveredCursorTextEvent[] {
    return this.drain(true);
  }

  private drain(flush: boolean): RecoveredCursorTextEvent[] {
    const events: RecoveredCursorTextEvent[] = [];

    while (this.buffer) {
      if (this.dropRedundantCloseTag()) continue;

      const start = this.buffer.search(/<tool_use\b/);
      if (start === -1) {
        const hold = flush ? 0 : toolUsePrefixSuffixLength(this.buffer);
        const end = this.buffer.length - hold;
        this.pushText(events, this.buffer.slice(0, end));
        this.buffer = this.buffer.slice(end);
        break;
      }

      if (start > 0) {
        this.pushText(events, this.buffer.slice(0, start));
        this.buffer = this.buffer.slice(start);
        continue;
      }

      const closeStart = this.buffer.indexOf("</tool_use>");
      if (closeStart === -1) {
        if (flush) {
          this.pushText(events, this.buffer);
          this.buffer = "";
        }
        break;
      }

      const closeEnd = closeStart + "</tool_use>".length;
      const raw = this.buffer.slice(0, closeEnd);
      const parsed = this.parseToolUse(raw);
      if (parsed) {
        this.recoveredToolUse = true;
        events.push(parsed);
      } else {
        this.pushText(events, raw);
      }
      this.buffer = this.buffer.slice(closeEnd);
    }

    return events;
  }

  private parseToolUse(raw: string): RecoveredCursorToolUse | undefined {
    const match = raw.match(/^<tool_use\b([^>]*)>([\s\S]*?)<\/tool_use>$/);
    if (!match) return undefined;
    const attrs = parseXmlAttributes(match[1] ?? "");
    const name = attrs.name;
    if (!name) return undefined;
    if (this.allowedToolNames && !this.allowedToolNames.has(name)) return undefined;

    let input: unknown;
    try {
      input = JSON.parse((match[2] ?? "").trim() || "{}");
    } catch {
      return undefined;
    }
    if (!isJsonObject(input)) return undefined;

    return {
      type: "tool_use",
      id: this.idFactory(),
      originalId: attrs.id,
      name,
      input,
    };
  }

  private pushText(events: RecoveredCursorTextEvent[], text: string): void {
    if (!text) return;
    if (this.recoveredToolUse && text.trim() === "") return;
    events.push({ type: "text", text });
  }

  private dropRedundantCloseTag(): boolean {
    if (!this.recoveredToolUse) return false;
    const match = this.buffer.match(/^\s*<\/tool_use>\s*/);
    if (!match?.[0]) return false;
    this.buffer = this.buffer.slice(match[0].length);
    return true;
  }
}

function parseXmlAttributes(source: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  const attrPattern = /([A-Za-z_][\w:.-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')/g;
  for (const match of source.matchAll(attrPattern)) {
    const key = match[1];
    const value = match[2] ?? match[3] ?? "";
    if (key) attrs[key] = decodeXmlAttribute(value);
  }
  return attrs;
}

function decodeXmlAttribute(value: string): string {
  return value
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&amp;", "&");
}

function isJsonObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function toolUsePrefixSuffixLength(value: string): number {
  const marker = "<tool_use";
  const max = Math.min(marker.length - 1, value.length);
  for (let len = max; len > 0; len--) {
    if (value.endsWith(marker.slice(0, len))) return len;
  }
  return 0;
}

function newCursorToolUseId(): string {
  return `call_cursor_${crypto.randomUUID().replaceAll("-", "")}`;
}
