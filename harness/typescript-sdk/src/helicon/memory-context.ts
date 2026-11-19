/**
 * memory_get's conversation part: where a memory was said (its source turns, hs
 * /v1/memory/source/<id>), the archived messages around that (hs /v1/memory/session/turns) — both
 * sides of the exchange, the assistant's replies too, which recall does not show — or the whole
 * conversation page by page; and how that is put in front of the model.
 */

import type { SessionTurn, SessionTurnsPage, SourceChain } from "./helicon-client.js";

/** Messages shown on each side of the source by default (memory_get window). */
export const DEFAULT_CONTEXT_WINDOW = 3;
/** At most this many on each side: more of the conversation is read with context="session". */
export const MAX_CONTEXT_WINDOW = 20;
/** Messages per page when the whole conversation is read. */
export const SESSION_PAGE_SIZE = 40;

/** {session id: the turn_index of each message the memory was taken from}, sessions in source order. */
export function sourceTurns(source: SourceChain | null): Map<string, number[]> {
  const out = new Map<string, number[]>();
  const add = (sid: unknown, idx: unknown) => {
    if (typeof sid !== "string" || !sid || typeof idx !== "number" || !Number.isInteger(idx)) {
      return;
    }
    const list = out.get(sid) ?? [];
    if (!list.includes(idx)) {
      list.push(idx);
    }
    out.set(sid, list);
  };
  for (const t of source?.turns ?? []) {
    add(t.session_id, t.turn_index);
  }
  for (const e of source?.evidence ?? []) {
    add(e.session_id, e.turn_index);
  }
  for (const list of out.values()) {
    list.sort((a, b) => a - b);
  }
  return out;
}

/** The sessions a memory names as its source (source_refs), in order, whether or not a turn is known. */
export function sourceSessions(source: SourceChain | null, turns: Map<string, number[]>): string[] {
  const out = [...turns.keys()];
  for (const r of source?.source_refs ?? []) {
    if (r?.type === "session" && typeof r.ref_id === "string" && r.ref_id && !out.includes(r.ref_id)) {
      out.push(r.ref_id);
    }
  }
  return out;
}

/** A turn spec as recall's RAW_REFS lists it — "12", "12-14" (or a number) — as [first, last]. */
export function parseTurns(spec: unknown): [number, number] | null {
  if (typeof spec === "number" && Number.isInteger(spec) && spec >= 0) {
    return [spec, spec];
  }
  if (typeof spec !== "string") {
    return null;
  }
  const m = /^\s*(\d+)\s*(?:[-–~]\s*(\d+))?\s*$/.exec(spec);
  if (!m) {
    return null;
  }
  const a = Number(m[1]);
  const b = m[2] === undefined ? a : Number(m[2]);
  return a <= b ? [a, b] : [b, a];
}

/** The window shown around [first, last]: `window` messages on each side, clamped to [0, MAX]. */
export function windowAround(first: number, last: number, window: number): { from: number; to: number } {
  const w = Math.max(0, Math.min(MAX_CONTEXT_WINDOW, Math.floor(window)));
  return { from: Math.max(0, first - w), to: last + w };
}

/** What the marked messages are: where the memory was taken from, or the excerpt asked for. */
export type Marked = "memory source" | "excerpt";

/** One archived message as shown: its index, who said it, when, and all it says. */
export function formatTurn(t: SessionTurn, marked: ReadonlySet<number>, what: Marked = "memory source"): string {
  const head = `[#${t.turn_index}] ${t.role}${t.created_at ? ` · ${t.created_at}` : ""}` +
    (marked.has(t.turn_index) ? (what === "excerpt" ? "  ◀ the recalled excerpt" : "  ◀ source of this memory") : "");
  const body = t.omitted ? "(removed — its user asked for it to be forgotten)" : t.content;
  return `${head}\n${body}`;
}

/** A page of a conversation as shown, with where it lies in the session and how to read on. */
export function formatPage(params: {
  sessionId: string;
  page: SessionTurnsPage;
  sourceTurns: readonly number[];
  mode: "around" | "session";
  window: number;
  marked?: Marked;
}): string {
  const { sessionId, page, mode, window } = params;
  const src = new Set(params.sourceTurns);
  if (page.turns.length === 0) {
    return `(no archived messages of session ${sessionId} in that range — it holds ${page.total}` +
      (page.first_turn !== null ? `, turns #${page.first_turn}–#${page.last_turn}` : "") + ")";
  }
  const a = page.turns[0]!.turn_index;
  const b = page.turns[page.turns.length - 1]!.turn_index;
  const span = page.first_turn !== null ? ` (the session holds ${page.total} messages, #${page.first_turn}–#${page.last_turn})` : "";
  const head = mode === "around"
    ? `--- Conversation around it: session ${sessionId}, messages #${a}–#${b}${span} ---`
    : `--- Conversation: session ${sessionId}, messages #${a}–#${b}${span} ---`;
  const tail: string[] = [];
  if (mode === "session" && page.more) {
    tail.push(`(more follows: call helicon__memory_get again with context="session", session="${sessionId}", fromTurn=${b + 1})`);
  }
  if (mode === "around") {
    const before = page.first_turn !== null && a > page.first_turn;
    const after = page.more || (page.last_turn !== null && b < page.last_turn);
    if (before || after) {
      tail.push(
        `(the conversation goes on ${before && after ? "before and after" : before ? "before" : "after"} this: ` +
        (window < MAX_CONTEXT_WINDOW ? `window=${Math.min(MAX_CONTEXT_WINDOW, window * 2 || 1)} shows more around it, ` : "") +
        `context="session" reads all of it)`,
      );
    }
  }
  return [head, ...page.turns.map((t) => formatTurn(t, src, params.marked)), ...tail].join("\n\n");
}
