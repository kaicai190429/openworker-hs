/**
 * The SessionBridge as this plugin shows it: the text put into the system prompt once per session
 * (the same string across turns, so the provider can cache the prompt prefix), and what that text
 * shows, item by item (hs bridge.v2) — what to ask /session_bridge/check about before reusing it,
 * and what of it is shown whole, so the per-turn recall does not show it a second time.
 */

import type { BridgeCheckItem, BridgeConflict, ConflictFact, SessionBridgeData } from "./helicon-client.js";

/** The SessionBridge protocol read here: items carry versions and can be checked. */
export const BRIDGE_PROTOCOL = "bridge.v2";
/** A statement of a recorded conflict is shown up to this many characters. */
const CONFLICT_CLAIM_CHARS = 120;

/**
 * What a SessionBridge text shows. `items`: every item it shows, whole or in part — what the check
 * is asked about. `complete`: the items it shows whole, by key ("memory:<id>", "topic:<name>",
 * "session_summary:<memory id>"), with the version shown — a recall block of the same item, in
 * the same version, showing nothing more, is shown already.
 */
export type BridgeShown = {
  items: BridgeCheckItem[];
  complete: Map<string, { version: string; coverage?: string | null }>;
};

export type BridgeRendering = { text: string; shown: BridgeShown };

/** Nothing to show. */
const empty = (): BridgeRendering => ({ text: "", shown: { items: [], complete: new Map() } });

type Line = {
  line: string;
  /** shown whole (not cut to the per-item limit) */
  whole: boolean;
  item: BridgeCheckItem;
  key: string;
  version: string;
  coverage?: string | null;
  /** the newest conflicts recorded on it (the server names them per item) */
  conflictIds: string[];
};

/**
 * Format SessionBridge data into a system-prompt string within a token budget, and say what it
 * shows.
 *
 * Two-phase content-proportional allocation:
 *   Phase 1 (measure): collect all items with per-item snippet limits, compute each section's
 *     desired size (how much it would use uncapped).
 *   Phase 2 (allocate): all content fits → everything; otherwise each section gets
 *     (section_desired / total_desired) * available_budget — empty sections get 0.
 *   Phase 3 (cascade): unused allocation rolls over to the next section (ep → pin → proc).
 *
 * Priority order (most valuable first):
 *   1. recent_episodes  — "what happened recently"
 *   2. pinned_semantics — "known facts about you/project" (topic summaries when there are any)
 *   3. procedures       — "how to do things"
 *
 * Then the conflicts the local model recorded on what is shown: a fact or a topic summary shown is
 * not to stand alone as the current arrangement when another stored statement may say otherwise.
 * Neither the conflict nor any verdict on it is passed on (the local model's record, which may be
 * wrong): each pair is listed as its two statements in the order they were said, each with when it
 * was said, for the agent to judge. They are not cut by the budget — that would leave the fact
 * standing alone — but are bounded: the newest `conflict_limit` of them, each statement cut to a line.
 * Returns empty text when bridge data has no useful content (is_empty).
 */
export function formatBridgeContext(bridge: SessionBridgeData, tokenBudget: number): BridgeRendering {
  if (bridge.is_empty) {
    return empty();
  }

  // 2.0 chars/token: conservative for mixed Chinese/English tech content
  // (English identifiers inflate the effective ratio vs pure Chinese ~1.5).
  const charBudget = Math.floor(tokenBudget * 2.0);

  const MAIN_HEADER  = "## 跨会话记忆\n";
  const EP_TITLE     = "**近期会话摘要**\n";
  const PIN_TITLE    = "**关于你/项目的已知事实**\n";
  const PROC_TITLE   = "**当前项目工作流**\n";
  const RELATED_TITLE = "**相关的已存陈述（各附说的时间）**\n";

  // ── Phase 1: Collect formatted lines (apply per-item snippet limits) ─────
  // Snippet limits are chosen to capture ~90% of real content for each type:
  //   episodic  1200: complex session summaries can run 500-2000 chars
  //   pinned     300: multi-attribute semantic facts
  //   procedural 500: numbered workflow steps with shell commands
  // A summary made before its session went on says so: the turns after it are not in it.
  const epLines: Line[] = bridge.recent_episodes.map((ep) => {
    const later = ep.session_summary.stale ? ep.session_summary.later_turns : 0;
    const note = later > 0 ? `（此摘要之后该会话又有 ${later} 条消息，未计入）` : "";
    const info = ep.session_summary;
    return {
      line: `- ${ep.text.slice(0, 1200).replace(/\n/g, " ")}${note}\n`,
      whole: ep.text.length <= 1200,
      item: { kind: "session_summary" as const, session_id: info.session_id, memory_id: ep.id,
              generation: info.generation, stale: info.stale },
      key: `session_summary:${ep.id}`,
      version: ep.version,
      conflictIds: [],
    };
  });
  // The topic summaries (Category Layer) when the scope has any; until it does, the pinned facts.
  // (A topic summary some of what it summarised no longer stands is not returned until it is made
  // again: hs session_routes_v2.get_session_bridge.)
  const hasCategorySummaries = bridge.category_summaries.length > 0;
  const pinLines: Line[] = hasCategorySummaries
    ? bridge.category_summaries.map((cs) => ({
        line: `- [${cs.topic_name}] ${cs.summary.slice(0, 200).replace(/\n/g, " ")}\n`,
        whole: cs.summary.length <= 200,
        item: { kind: "topic" as const, topic_name: cs.topic_name, version: cs.version,
                coverage: cs.coverage?.version ?? null },
        version: cs.version,
        coverage: cs.coverage?.version ?? null,
        key: `topic:${cs.topic_name}`,
        conflictIds: cs.conflict_ids,
      }))
    : bridge.pinned_semantics.map((m) => ({
        line: `- [${m.category}] ${m.text.slice(0, 300).replace(/\n/g, " ")}\n`,
        whole: m.text.length <= 300,
        item: { kind: "memory" as const, memory_id: m.id, version: m.version },
        version: m.version,
        key: `memory:${m.id}`,
        conflictIds: m.conflict_ids,
      }));
  const procLines: Line[] = bridge.procedures.map((p) => ({
    line: `- ${p.text.slice(0, 500).replace(/\n/g, " ")}\n`,
    whole: p.text.length <= 500,
    item: { kind: "memory" as const, memory_id: p.id, version: p.version },
    version: p.version,
    key: `memory:${p.id}`,
    conflictIds: p.conflict_ids,
  }));

  // Desired size = title + all lines (0 if section is empty — no wasted budget)
  const sumLen = (lines: Line[]) => lines.reduce((s, l) => s + l.line.length, 0);
  const epDesired   = epLines.length   > 0 ? EP_TITLE.length   + sumLen(epLines)   : 0;
  const pinDesired  = pinLines.length  > 0 ? PIN_TITLE.length  + sumLen(pinLines)  : 0;
  const procDesired = procLines.length > 0 ? PROC_TITLE.length + sumLen(procLines) : 0;
  const contentDesired = epDesired + pinDesired + procDesired;
  const totalDesired   = MAIN_HEADER.length + contentDesired;

  if (contentDesired === 0) {
    return empty();
  }

  // ── Phase 2: Compute per-section budget ──────────────────────────────────
  const available = charBudget - MAIN_HEADER.length; // chars left after main header
  let epAlloc: number, pinAlloc: number, procAlloc: number;

  if (totalDesired <= charBudget) {
    // All content fits — allocate exactly what each section needs
    epAlloc   = epDesired;
    pinAlloc  = pinDesired;
    procAlloc = procDesired;
  } else {
    // Proportional allocation: each section gets its fair share of available chars.
    // Empty sections (desired=0) get 0 automatically — no wasted budget.
    epAlloc   = Math.floor((epDesired   / contentDesired) * available);
    pinAlloc  = Math.floor((pinDesired  / contentDesired) * available);
    procAlloc = available - epAlloc - pinAlloc; // remainder to avoid rounding loss
  }

  // ── Phase 3: Fill sections, cascade unused allocation ────────────────────
  // fill() returns [chars_actually_used, the lines shown].
  // Orphaned section headers are prevented: title is included only if items fit.
  const fill = (title: string, lines: Line[], budget: number): [number, Line[]] => {
    if (lines.length === 0 || budget <= 0) {
      return [0, []];
    }
    const buf: Line[] = [];
    let used = title.length; // title always costs chars
    for (const line of lines) {
      if (used + line.line.length > budget) {
        break;
      }
      buf.push(line);
      used += line.line.length;
    }
    if (buf.length === 0) {
      return [0, []]; // no items fit → don't emit the title either
    }
    return [used, buf];
  };

  const [epUsed,   epShown]   = fill(EP_TITLE,   epLines,   epAlloc);
  // Cascade: ep leftover rolls into pin's budget
  const [pinUsed,  pinShown]  = fill(PIN_TITLE,  pinLines,  pinAlloc + (epAlloc - epUsed));
  // Cascade: pin leftover rolls into proc's budget
  const [,         procShown] = fill(PROC_TITLE, procLines, procAlloc + (pinAlloc + (epAlloc - epUsed) - pinUsed));

  const sections: string[] = [];
  for (const [title, shown] of [[EP_TITLE, epShown], [PIN_TITLE, pinShown], [PROC_TITLE, procShown]] as const) {
    if (shown.length > 0) {
      sections.push(title, ...shown.map((l) => l.line));
    }
  }
  if (sections.length === 0) {
    return empty();
  }

  const all = [...epShown, ...pinShown, ...procShown];
  // the conflicts recorded on what is shown: of those its items name, the newest (by when recorded,
  // then id — the order the server checks them in), as many as the server says to show
  const named = new Set(all.flatMap((l) => l.conflictIds));
  const conflicts: BridgeConflict[] = bridge.conflicts
    .filter((c) => named.has(c.relation_id))
    .sort((a, b) => (b.recorded_at ?? 0) - (a.recorded_at ?? 0)
      || (a.relation_id < b.relation_id ? -1 : a.relation_id > b.relation_id ? 1 : 0))
    .slice(0, bridge.conflict_limit);
  if (conflicts.length > 0) {
    // each statement with when it was said — first and last, when it was said again since: the one
    // said first is not the earlier of the two if it was said again after the other — the two in
    // the order they were (last) said, as hs recall_render._said_order shows a recall's
    const said = (f: ConflictFact) =>
      f.first_said_at ? `${f.first_said_at}，最近一次 ${f.said_at ?? "未知"}` : (f.said_at ?? "未知");
    const order = (a: ConflictFact, b: ConflictFact) => (a.said_ts ?? 0) - (b.said_ts ?? 0)
      || ((a.said_at ?? "") < (b.said_at ?? "") ? -1 : (a.said_at ?? "") > (b.said_at ?? "") ? 1 : 0);
    const claim = (s: string) => s.slice(0, CONFLICT_CLAIM_CHARS).replace(/\n/g, " ");
    sections.push(RELATED_TITLE, ...conflicts.map((c) =>
      `- ${[...c.facts].sort(order).map((f) => `「${claim(f.claim)}」（说于 ${said(f)}）`).join("；")}\n`));
  }
  const text = MAIN_HEADER + sections.join("");

  const complete = new Map<string, { version: string; coverage?: string | null }>();
  for (const l of all) {
    if (l.whole) {
      complete.set(l.key, { version: l.version, ...(l.coverage !== undefined ? { coverage: l.coverage } : {}) });
    }
  }
  const items: BridgeCheckItem[] = [
    ...all.map((l) => l.item),
    ...conflicts.map((c) => ({ kind: "conflict" as const, relation_id: c.relation_id })),
  ];
  return { text, shown: { items, complete } };
}

/**
 * A turn's SessionBridge as it ends up (§8, the freeze barrier): validated from the cache (checked
 * this turn), validated fresh (fetched this turn), or omitted — not injected this turn, and not
 * what the recall leaves out by (why: off, empty, fetch / check failed, late).
 */
export type BridgeTurn =
  | { state: "validated_cached" | "validated_fresh"; rendering: BridgeRendering }
  | { state: "omitted"; why: string };

/**
 * The bridge as it stands at byMs: what it settled to, or omitted ("late") when it has not settled
 * by then. One answer per turn: the recall's composing and the bridge's injection both take it.
 */
export function freezeBridgeBy(bridge: Promise<BridgeTurn>, byMs: number): Promise<BridgeTurn> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<BridgeTurn>((resolve) => {
    timer = setTimeout(() => resolve({ state: "omitted", why: "late" }), Math.max(0, byMs - Date.now()));
  });
  // (a bridge already settled wins even when byMs has passed: its reaction is queued before the timer)
  return Promise.race([bridge, late]).finally(() => clearTimeout(timer));
}
