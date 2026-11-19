/**
 * Reads a session's durable history from the engine (`history/read`) and turns it into the
 * message shape the archive and capture code was written for.
 *
 * The engine's history is append-only and each item has a dense, increasing `index`. That index is
 * what HeliconSearch gets as `turn_index`: it never shifts (compaction trims what the model sees,
 * not this list), so unlike the OpenClaw plugin no transcript-format reader or outbox is needed.
 *
 * Items the model produced or received, as the engine stores them:
 *   user                 {role:"user", content: string | blocks}
 *   assistant text       {type:"message", role:"assistant", content:[{type:"output_text", text}]}
 *   tool call            {type:"function_call", call_id, name, arguments}
 *   tool result          {type:"function_call_output", call_id, output}
 * Reasoning items are never returned. The per-turn application addendum (recalled memory) is not
 * part of the history, so nothing injected needs stripping.
 */

export type HistoryRequest = (method: string, params: unknown) => Promise<unknown>;
export type HistoryItem = { index: number; item: Record<string, unknown> };
export type HistoryPage = {
  items: HistoryItem[];
  /** the next index to read from */
  next: number;
  /** the read stopped (a very long history) with more left: `next` is not the end of the history */
  more: boolean;
};

const MAX_PAGES = 200;

function rec(v: unknown): Record<string, unknown> | undefined {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
}

/** Every history item from index `after` on. */
export async function readHistory(request: HistoryRequest, sessionId: string, after: number): Promise<HistoryPage> {
  const items: HistoryItem[] = [];
  let cursor = Math.max(0, after);
  let moreLeft = false;
  for (let page = 0; page < MAX_PAGES; page++) {
    const r = rec(await request("history/read", { session_id: sessionId, after: cursor }));
    if (!r) break;
    for (const entry of Array.isArray(r.items) ? r.items : []) {
      const e = rec(entry);
      const item = rec(e?.item);
      if (e && item && typeof e.index === "number") items.push({ index: e.index, item });
    }
    const next = typeof r.cursor === "number" ? r.cursor : cursor;
    const more = r.has_more === true && next > cursor;
    cursor = next;
    moreLeft = more;
    if (!more) break;
  }
  return { items, next: cursor, more: moreLeft };
}

export type AdaptContext = {
  /** model-facing name prefix of this adapter's own tools (e.g. "helicon__"): never archived */
  ownToolPrefix: string;
  /** call ids of this adapter's tool calls seen so far (their results are skipped too) */
  ownCallIds: Set<string>;
  /** stamped on every message: the history carries no per-item time */
  now: number;
};

type Block = { type: string; text?: string; name?: string };

function blocksOf(content: unknown): Block[] {
  if (typeof content === "string") return [{ type: "text", text: content }];
  if (!Array.isArray(content)) return [];
  const out: Block[] = [];
  for (const b of content) {
    const block = rec(b);
    if (!block) continue;
    const type = String(block.type ?? "");
    if ((type === "input_text" || type === "output_text" || type === "text") && typeof block.text === "string") {
      out.push({ type: "text", text: block.text });
    } else if (type === "input_image" || type === "image") {
      out.push({ type: "image" });
    } else if (type === "input_file" || type === "document") {
      out.push({ type: "document", ...(typeof block.filename === "string" ? { name: block.filename } : {}) });
    }
  }
  return out;
}

/**
 * One history item as a conversation message, or undefined when it is not part of what is
 * archived (reasoning, runtime notices, this adapter's own tool traffic).
 */
export function adaptItem(item: Record<string, unknown>, ctx: AdaptContext): Record<string, unknown> | undefined {
  const timestamp = ctx.now;
  const type = typeof item.type === "string" ? item.type : null;
  if (type === "function_call") {
    const name = typeof item.name === "string" ? item.name : "unknown_tool";
    const callId = typeof item.call_id === "string" ? item.call_id : "";
    if (name.startsWith(ctx.ownToolPrefix)) {
      if (callId) ctx.ownCallIds.add(callId);
      return undefined;
    }
    let input: unknown = {};
    try {
      input = typeof item.arguments === "string" ? JSON.parse(item.arguments) : (item.arguments ?? {});
    } catch {
      input = { arguments: item.arguments };
    }
    return { role: "assistant", content: [{ type: "tool_use", id: callId, name, input }], timestamp };
  }
  if (type === "function_call_output") {
    const callId = typeof item.call_id === "string" ? item.call_id : "";
    if (ctx.ownCallIds.has(callId)) return undefined;
    const output = typeof item.output === "string" ? item.output : JSON.stringify(item.output ?? "");
    return { role: "tool", content: [{ type: "tool_result", tool_use_id: callId, content: output }], timestamp };
  }
  const role = item.role;
  if (role !== "user" && role !== "assistant") return undefined;
  if (item.runtime_notice) return undefined; // injected by the runtime, not said by anyone
  const blocks = blocksOf(item.content);
  if (blocks.length === 0) return undefined;
  return { role, content: blocks, timestamp };
}

/**
 * A sparse message array indexed by history index (holes for what is not archived), the shape the
 * archive/capture helpers walk with `for (i = start; i < messages.length; i++)`.
 */
export function toMessages(page: HistoryPage, ctx: AdaptContext): unknown[] {
  const messages: unknown[] = new Array(page.next);
  for (const { index, item } of page.items) {
    const m = adaptItem(item, ctx);
    if (m) messages[index] = m;
  }
  return messages;
}
