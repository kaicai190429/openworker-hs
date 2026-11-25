/**
 * Auto-recall: query heliconsearch on `before_prompt_build` and produce a
 * `prependContext` block to inject into the agent prompt.
 *
 * Fail-closed: any error path returns {} so prompt construction is never
 * blocked by an offline / slow heliconsearch.
 */

import { createHash } from "node:crypto";
import type { MemoryConfig } from "./config.js";
import {
  isAutomatedTurn,
  normalizeRecallQuery,
  wrapRelevantMemoriesBlock,
} from "./filters.js";
import {
  type ClientLogger,
  type HeliconClient,
  type PromptBlockRef,
  type QueryResponse,
  type RawContextSession,
  type RawTurn,
  type RelatedSkill,
  MEMORY_ID_PATTERN,
  heliconErrorKind,
  pickAddressableMemoryId,
} from "./helicon-client.js";
import { type HookBudget, MIN_RECALL_BUDGET_MS, type RecallBudgets } from "./recall-budget.js";
import type { BridgeShown } from "./session-bridge.js";

/**
 * Build a structured `<recalled_skills>...</recalled_skills>` block listing
 * skills HS returned in `related_skills` (top of recall prompt, beside the
 * server-rendered trust band). Text-only because `before_prompt_build`'s
 * `prependContext` accepts a single string; the XML shape mirrors openclaw's
 * distinct from openclaw's native `<available_skills>` so the model does not
 * conflate remote HS skills with local skill files, and a future UI renderer
 * can parse it.
 */
function escapeXml(str: string): string {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

function buildHeliconSkillsBlock(skills: RelatedSkill[]): { text: string; count: number } {
  if (skills.length === 0) {
    return { text: "", count: 0 };
  }
  const lines: string[] = [];
  // Deliberately does NOT reuse openclaw's native `<available_skills>` tag
  // (src/agents/skills/skill-contract.ts:formatSkillsForPrompt). That block
  // tells the model to `read` a skill file at a local <location> path; if we
  // shared the tag the model conflates the two and fabricates a filesystem
  // path like .openclaw/plugin-skills/<name>/SKILL.md instead of calling
  // skill_get. These skills are remote HS records, not local files — hence a
  // distinct <recalled_skills> tag and an explicit no-read instruction.
  // HS-side recall is query-scoped: only matching skills appear, each with a
  // confidence <score> and a 32-hex <skill_id>.
  lines.push("");
  lines.push("The following skills were recalled by HeliconSearch for the current query.");
  lines.push(
    "These are REMOTE records, NOT local files. Do NOT try to read them from disk and do NOT guess a filesystem path: no such file exists.",
  );
  lines.push(
    "The ONLY way to load a recalled skill's SKILL.md body is the `helicon__skill_get(skillId)` tool, passing the 32-hex `<skill_id>` verbatim. " +
        "NEVER call `helicon__memory_get` for a `<skill_id>` — it returns 404; only `helicon__skill_get` accepts skill ids.",
  );
  lines.push(
    "Do NOT fetch these skills via `web_fetch` or `curl` — the server URL is private and exposing it would leak the host.",
  );
  lines.push(
    "helicon__skill_get returns the SKILL.md markdown body. Parse it and follow the skill's guidance.",
  );
  lines.push("");
  lines.push("If one clearly applies, call helicon__skill_get with its <skill_id>, then follow the skill.");
  lines.push("If several apply, choose the most specific and only fetch that one.");
  lines.push("If none clearly apply, fetch none.");
  lines.push("One skill up front max — do not speculatively call helicon__skill_get for multiple skills in parallel.");
  lines.push("Never invent skill ids — only use the <skill_id> values listed in <recalled_skills> below.");
  lines.push("");
  lines.push("<recalled_skills>");
  for (const s of skills) {
    const score = typeof s.score === "number" ? s.score.toFixed(3) : null;
    const desc = (s.description ?? "").trim();
    lines.push("  <recalled_skill>");
    lines.push(`    <skill_id>${escapeXml(s.skill_id)}</skill_id>`);
    lines.push(`    <name>${escapeXml(s.name)}</name>`);
    if (score !== null) {
      lines.push(`    <score>${escapeXml(score)}</score>`);
    }
    if (desc) {
      lines.push(`    <description>${escapeXml(desc)}</description>`);
    }
    lines.push("  </recalled_skill>");
  }
  lines.push("</recalled_skills>");
  return { text: "\n\n" + lines.join("\n"), count: skills.length };
}

const PLUGIN_ID = "helicon-memory";
/** Max ids listed in the MEMORY_IDS index appended to injected context (keeps the block ~5 short lines). */
const MEMORY_ID_INDEX_LIMIT = 5;
const MEMORY_ID_INDEX_CLAIM_CHARS = 60;

/**
 * Build a compact id index for the injected recall block.
 *
 * The server-rendered `prompt_text` deliberately carries no ids, so an agent reading injected
 * memories has no handle to expand anything — it can see a fact but cannot ask for its full
 * record or provenance. This lists the addressable memory ids (32-hex) of the strongest facts
 * so memory_get becomes reachable without a prior memory_recall call.
 *
 * Deliberately worded as a data label, not as an instruction: this block is injected inside the
 * `<relevant-memories>` envelope, whose preamble tells the model not to follow instructions found
 * in memories. Telling it what to do with these ids belongs in the (trusted) system prompt
 * section built by registerMemoryCapability — this side only supplies the id ↔ fact mapping.
 * The claims stay inside the envelope because they are memory-derived, i.e. untrusted text.
 *
 * Built client-side on purpose: the hs renderer is shared with the LoCoMo benchmark, and
 * changing it would change evaluation prompts.
 */
function memoryIdIndex(rows: { memoryId: string | null; claim: string }[]): { text: string; count: number } {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const row of rows) {
    // 多条 fact 可能派生自同一条记忆——同一个 id 不重复占用索引名额。
    const memoryId = (row.memoryId ?? "").trim().toLowerCase();
    if (!MEMORY_ID_PATTERN.test(memoryId) || seen.has(memoryId)) {
      continue;
    }
    seen.add(memoryId);
    const claim = row.claim.replace(/\s+/g, " ").trim().slice(0, MEMORY_ID_INDEX_CLAIM_CHARS);
    out.push(`- ${memoryId} — ${claim}`);
    if (out.length >= MEMORY_ID_INDEX_LIMIT) {
      break;
    }
  }
  if (out.length === 0) {
    return { text: "", count: 0 };
  }
  return {
    text: `\n\n## MEMORY_IDS  (memory ids of the ${out.length} facts above)\n${out.join("\n")}`,
    count: out.length,
  };
}

/**
 * Where each verbatim excerpt of the injected block is (its [Rn] label → session + turns), from
 * the server's raw blocks: an excerpt has no memory id, and memory_get opens it by where it is,
 * with the rest of its exchange (recall shows the user's side only). A data label, like
 * MEMORY_IDS — what to do with it is the trusted system prompt's to say.
 */
function rawRefIndex(refs: { label: string; session: string; turns: string | null }[]): { text: string; count: number } {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const r of refs) {
    if (!r.label || !r.session || seen.has(r.label)) {
      continue;
    }
    seen.add(r.label);
    out.push(`- ${r.label} — session="${r.session}"${r.turns !== null ? `, turns="${r.turns}"` : ""}`);
  }
  if (out.length === 0) {
    return { text: "", count: 0 };
  }
  return { text: `\n\n## RAW_REFS  (where the ${out.length} verbatim excerpt(s) above are)\n${out.join("\n")}`, count: out.length };
}

/** The prompt_blocks protocol this plugin reads. */
/**
 * Excerpts the `memory_recall` tool adds beside the facts: windows of turns whose facts have not been extracted yet. The server
 * searches them for every recall (the newest turns), but they are in `raw_context`, which the tool used to show only when
 * there was no fact at all — so a question about what was said a moment ago, with unrelated facts around, never saw them.
 * Best-scored first; the number is a bound on the tool's output, not a quality cut (the answer was inside the best five in
 * every case the server found it at all).
 */
export const UNEXTRACTED_EXCERPTS_MAX = 5;

export type UnextractedExcerpt = { sessionId: string; turn: RawTurn };

export function pickUnextractedExcerpts(
  raw: RawContextSession[] | undefined,
  minScore: number,
  max: number = UNEXTRACTED_EXCERPTS_MAX,
): UnextractedExcerpt[] {
  const all: UnextractedExcerpt[] = [];
  for (const s of raw ?? []) {
    for (const t of s.turns ?? []) {
      if (t.is_orphan === true && (t.score ?? 0) >= minScore && typeof t.text === "string" && t.text.trim() !== "") {
        all.push({ sessionId: s.session_id, turn: t });
      }
    }
  }
  // (stable: equal scores keep the server's order)
  return all
    .map((x, i) => ({ x, i }))
    .sort((a, b) => (b.x.turn.score ?? 0) - (a.x.turn.score ?? 0) || a.i - b.i)
    .slice(0, Math.max(0, max))
    .map((e) => e.x);
}

export function formatUnextractedExcerpts(items: UnextractedExcerpt[]): string {
  if (items.length === 0) return "";
  return (
    "Verbatim excerpts from the newest turns (not yet extracted into memories, so they carry no id; helicon__memory_get with the session " +
    "+ turns given opens each with the rest of its exchange):\n" +
    items
      .map(
        (x, i) =>
          `${i + 1}. ${x.turn.text}` +
          (x.turn.turn !== null && x.turn.turn !== undefined
            ? ` (session="${x.sessionId}", turns="${String(x.turn.turn)}")`
            : ` (session="${x.sessionId}")`),
      )
      .join("\n")
  );
}

export const BLOCKS_PROTOCOL = "blocks.v1";

/** The key a recall block's item is shown under in the SessionBridge, if it can be shown there. */
function shownKey(ref: PromptBlockRef): string | null {
  if (ref.type === "memory" && ref.memory_id) {
    return `memory:${ref.memory_id}`;
  }
  if (ref.type === "topic" && ref.topic_name) {
    return `topic:${ref.topic_name}`;
  }
  if (ref.type === "session_summary" && ref.memory_id) {
    return `session_summary:${ref.memory_id}`;
  }
  return null;
}

/** An id-only reference to an item shown whole in the SessionBridge already. */
function reference(ref: PromptBlockRef): string {
  if (ref.type === "topic") {
    return `\n• (topic "${ref.topic_name}": shown above under 跨会话记忆)`;
  }
  if (ref.type === "session_summary") {
    return `\n• (summary of ${ref.session_id}: shown above under 跨会话记忆)`;
  }
  const id = ref.type === "memory" ? ref.memory_id : "";
  return `\n• (memory ${id}: shown above under 跨会话记忆)`;
}

export type ComposedRecall = {
  /** what to inject (before the MEMORY_IDS index and the skills block) */
  text: string;
  idIndex: { text: string; count: number };
  /** where the verbatim excerpts shown are (RAW_REFS) */
  rawIndex: { text: string; count: number };
  /** blocks left out: the SessionBridge shows the same item whole, in the same version */
  removed: number;
  /** the same item in the same version, kept: the block shows more (a quote, a time, a note) */
  keptMore: number;
  /** the same item in another version, kept */
  otherVersion: number;
  /**
   * the blocks shown in full this time that have an identity (`memory:<id>`, `topic:<name>`, `legend`), with the hash of
   * their text: what the session's conversation holds of the recall once this turn is in it
   */
  injected: Array<[string, string]>;
  /** blocks left out because the session's conversation already holds exactly their text */
  repeated: number;
};

/** What a session's conversation already holds of the recall, block by block: the block's identity, the hash of the text shown (`recallReference`). */
export type SeenBlocks = Map<string, string>;

function textHash(text: string): string {
  return createHash("sha1").update(text).digest("hex");
}

/** The identity of a recall block, if it has one: the items the SessionBridge can show, and the legend (the same text every time). */
function blockKey(ref: PromptBlockRef): string | null {
  return ref.type === "legend" ? "legend" : shownKey(ref);
}

/** What names an item in a pointer: the first 8 characters of a memory id (the full ids were listed when it was first shown), a topic's name, a summary's session. */
function itemName(ref: PromptBlockRef): string {
  if (ref.type === "topic") return `topic "${ref.topic_name}"`;
  if (ref.type === "session_summary") return `summary of ${ref.session_id}`;
  return (ref.type === "memory" ? ref.memory_id ?? "" : "").slice(0, 8);
}

/** A pointer to an item whose text the session's conversation already holds, exactly. */
function seenPointer(ref: PromptBlockRef): string {
  const name = itemName(ref);
  return ref.type === "topic" || ref.type === "session_summary"
    ? `\n• (${name}: shown earlier in this conversation)`
    : `\n• (memory ${name}: shown earlier in this conversation)`;
}

/**
 * The recalled context to inject this turn, from the server's blocks (prompt_blocks, protocol
 * blocks.v1, joining to exactly prompt_text): every block as it is, except one whose item the
 * SessionBridge shows whole in the same version (same id and version — and, for a topic summary,
 * the same coverage) while the block adds nothing to it (no time, quote, context or note): that
 * one becomes an id-only reference. MEMORY_IDS then lists the facts left in view: those the query
 * found at or above minScore, and every fact of a conflict unit (rendered as memory blocks; some
 * were not found by the query itself and are only in `conflicts`, without a score).
 * Nothing is taken apart by pattern; nothing is left out on a guess that a summary covers a fact.
 *
 * null when the blocks are missing, of another protocol, or do not make up prompt_text: the
 * server sends them with every prompt_text, so that is its fault — not passed over by injecting
 * prompt_text without them.
 */
export function composeRecall(
  resp: QueryResponse,
  shown: BridgeShown | null,
  minScore: number,
  seen?: SeenBlocks | null,
): ComposedRecall | null {
  const promptText = resp.prompt_text ?? "";
  const pb = resp.prompt_blocks;
  if (pb === undefined || pb.protocol !== BLOCKS_PROTOCOL || !Array.isArray(pb.blocks)
    || pb.blocks.map((b) => b.text).join("") !== promptText) {
    return null;
  }
  const facts = new Map<string, { claim: string; listed: boolean }>();
  for (const f of [...(resp.grounded_facts ?? []), ...(resp.needs_verification ?? [])]) {
    const id = pickAddressableMemoryId(f);
    if (id && !facts.has(id)) {
      facts.set(id, { claim: f.claim, listed: (f.score ?? 0) >= minScore });
    }
  }
  for (const u of resp.conflicts ?? []) {
    for (const f of u.facts) {
      facts.set(f.memory_id, { claim: facts.get(f.memory_id)?.claim ?? f.claim, listed: true });
    }
  }
  const out: string[] = [];
  const visible: { memoryId: string | null; claim: string }[] = [];
  const raws: { label: string; session: string; turns: string | null }[] = [];
  let removed = 0, keptMore = 0, otherVersion = 0;
  const injected: Array<[string, string]> = [];
  const repeatedNames: string[] = [];
  let newContent = 0;
  for (const b of pb.blocks) {
    const key = shownKey(b.ref);
    const there = key && shown ? shown.complete.get(key) : undefined;
    if (there) {
      const version = "version" in b.ref ? b.ref.version : null;
      const same = there.version === version
        && (b.ref.type !== "topic" || (there.coverage ?? null) === (b.ref.coverage ?? null));
      const extras = "extras" in b.ref ? b.ref.extras : [];
      if (same && extras.length === 0) {
        out.push(reference(b.ref));
        removed++;
        continue;
      }
      if (same) {
        keptMore++;
      } else {
        otherVersion++;
      }
    }
    const ownKey = blockKey(b.ref);
    const ownHash = textHash(b.text);
    if (seen && ownKey !== null && seen.get(ownKey) === ownHash) {
      // the same text is in the conversation already: the legend is not said again, an item is pointed to
      if (b.ref.type !== "legend") {
        out.push(seenPointer(b.ref));
        repeatedNames.push(itemName(b.ref));
      }
      continue;
    }
    if (ownKey !== null) injected.push([ownKey, ownHash]);
    if (b.ref.type !== "legend" && b.ref.type !== "header") newContent++;
    out.push(b.text);
    if (b.ref.type === "memory" && b.ref.memory_id) {
      const f = facts.get(b.ref.memory_id);
      if (f?.listed) {
        visible.push({ memoryId: b.ref.memory_id, claim: f.claim });
      }
    } else if (b.ref.type === "raw" && b.ref.label) {
      raws.push({ label: b.ref.label, session: b.ref.session_id,
                  turns: b.ref.turns === null || b.ref.turns === undefined ? null : String(b.ref.turns) });
    }
  }
  const repeated = repeatedNames.length;
  if (repeated > 0 && newContent === 0) {
    // nothing of this recall is new to the session
    return {
      text: `MEMORY RECALL — the memories found for this message (${repeatedNames.join(", ")}) were all shown earlier in this conversation.`,
      idIndex: { text: "", count: 0 },
      rawIndex: { text: "", count: 0 },
      removed, keptMore, otherVersion, injected: [], repeated,
    };
  }
  return { text: out.join("").trim(), idIndex: memoryIdIndex(visible), rawIndex: rawRefIndex(raws),
           removed, keptMore, otherVersion, injected, repeated };
}

/** What one turn's automatic recall did, for the hook's summary line. */
export type AutoRecallReport = {
  /**
   * injected | not_injected (nothing at or above the gate, or empty) | skipped (off, automated
   * turn, short or empty prompt) | no_budget (too little time left to send it) | failed |
   * bad_blocks (prompt_text without usable prompt_blocks)
   */
  outcome: "injected" | "not_injected" | "skipped" | "no_budget" | "failed" | "bad_blocks";
  why?: string;
  /** the budget_ms sent */
  budgetMs?: number;
  /** the server's execution: outcome / tier / retrieval mode ("none" when it ran no budgets) */
  server?: string;
  responseBytes?: number;
  /** content blocks injected whole (those the SessionBridge shows are left out), the injected text's length */
  blocks?: number;
  chars?: number;
  /** the failure's kind (deadline, network, unavailable, …) */
  errorKind?: string;
};

export type AutoRecallResult = {
  prependContext?: string;
  report: AutoRecallReport;
  /** the recall blocks put into this turn in full, by identity and text hash (`recallReference`) */
  shown?: Array<[string, string]>;
};

/**
 * The injection gate (§8): something at or above minScore among the facts and topic summaries, or a
 * block of raw turns not extracted yet, or a session-summary block. Low-scored facts alone are not
 * injected, whatever raw context came along unrendered; a qualified session summary alone is.
 */
export function passesInjectionGate(resp: QueryResponse, minScore: number): { pass: boolean; why: string } {
  const topScore = [...(resp.grounded_facts ?? []), ...(resp.topic_rollups ?? [])]
    .reduce((m, x) => Math.max(m, x.score ?? 0), 0);
  if (topScore >= minScore) {
    return { pass: true, why: `topScore ${topScore.toFixed(4)}` };
  }
  const blocks = resp.prompt_blocks?.blocks ?? [];
  const orphanRaw = blocks.filter((b) => b.ref.type === "raw" && b.ref.orphan).length;
  const summaries = blocks.filter((b) => b.ref.type === "session_summary").length;
  if (orphanRaw > 0 || summaries > 0) {
    return { pass: true, why: `topScore ${topScore.toFixed(4)} below minScore; unextracted raw blocks=${orphanRaw} session summaries=${summaries}` };
  }
  return { pass: false, why: `topScore ${topScore.toFixed(4)} below minScore ${minScore}, no unextracted raw or session summary block` };
}

export async function performAutoRecall(params: {
  client: HeliconClient;
  cfg: MemoryConfig;
  prompt: string;
  messages: unknown[];
  logger: ClientLogger;
  /** this turn's time (made at the start of before_prompt_build) */
  hookBudget: HookBudget;
  /** where the server's budgets are kept (updated from the response's execution) */
  budgets: RecallBudgets;
  signal?: AbortSignal;
  sessionKey?: string;
  /** ctx.trigger from the hook context; "heartbeat"/"cron" mean nobody is waiting. */
  trigger?: unknown;
  /**
   * What the SessionBridge injected this turn shows, as frozen for this turn (null: none injected —
   * not fetched, empty, not current, or not ready in time): items it shows whole are not shown
   * again (composeRecall). Asked only when composing; the same answer decides the bridge's injection.
   */
  bridgeShown?: () => Promise<BridgeShown | null>;
  /** `recallReference`: what the session's conversation already holds of the recall; blocks with the same text are pointed to, not shown again */
  seen?: SeenBlocks;
}): Promise<AutoRecallResult> {
  const startedMs = Date.now();
  const { client, cfg, prompt, messages, logger, signal, sessionKey, trigger, hookBudget, budgets } = params;

  logger.info(
    `[${PLUGIN_ID}] hook=before_prompt_build sessionKey=${
      sessionKey ?? "?"
    } promptLen=${prompt?.length ?? 0} historyMsgCount=${messages?.length ?? 0} autoRecall=${cfg.autoRecall}`,
  );

  if (!cfg.autoRecall) {
    return { report: { outcome: "skipped", why: "autoRecall off" } };
  }
  if (isAutomatedTurn(trigger)) {
    // A heartbeat or cron wake. Measured on a live instance: 487 of 955 recalls (51%) were
    // fired by the heartbeat template, each pulling ~57 rendered memories into a turn whose
    // whole job is to answer HEARTBEAT_OK -- and whose own prompt says "Do not infer or
    // repeat old tasks from prior chats". Deterministic on purpose: a 4B reads that template
    // as a genuine instruction and classifies it as real work.
    logger.info(
      `[${PLUGIN_ID}] hook=before_prompt_build → skipped (trigger=${String(trigger)}, not a user turn)`,
    );
    return { report: { outcome: "skipped", why: `trigger ${String(trigger)}` } };
  }
  if (!prompt || prompt.length < 5) {
    logger.info(
      `[${PLUGIN_ID}] hook=before_prompt_build → skipped (prompt too short, len=${prompt?.length ?? 0})`,
    );
    return { report: { outcome: "skipped", why: "prompt too short" } };
  }

  let queryText: string;
  try {
    // Use event.prompt (the current user message being sent) as the recall query.
    // event.messages is the history array and does NOT contain the current message,
    // so extractLatestUserText(messages) would return the previous turn's text.
    queryText = normalizeRecallQuery(prompt, cfg.recallMaxChars);
  } catch (err) {
    logger.warn(`[${PLUGIN_ID}] hook=before_prompt_build → query normalize failed: ${String(err)}`);
    return { report: { outcome: "failed", why: "query normalize failed" } };
  }

  if (!queryText) {
    logger.info(`[${PLUGIN_ID}] hook=before_prompt_build → empty query, skipping recall`);
    return { report: { outcome: "skipped", why: "empty query" } };
  }

  // what the server may take: the turn's deadline less the transport margin (it only shortens its
  // own budget to this); too little left and the recall could not end in time — not sent
  const budgetMs = hookBudget.recallBudgetMs();
  if (budgetMs < MIN_RECALL_BUDGET_MS) {
    logger.warn(
      `[${PLUGIN_ID}] auto_recall not sent: ${budgetMs}ms left for it (hook timeout ${hookBudget.hostTimeoutMs}ms, ` +
      `automatic budget ${hookBudget.budgets.autoMs}ms from ${hookBudget.budgets.source})`,
    );
    return { report: { outcome: "no_budget", budgetMs } };
  }

  // Log the recall query derived from the current user prompt
  logger.info(
    `[${PLUGIN_ID}] auto_recall querying memories for: "${queryText}" agent=${cfg.agentId} topK=${cfg.recallTopK} budgetMs=${budgetMs}`,
  );

  const meta: { bytes?: number } = {};
  try {
    // client.query() 直接返回 organized 视图 + 服务端渲染的 compact prompt_text（信任分带，
    // 与 benchmark/eval 消费的是同一渲染器，见 heliconsearch ORGANIZED_RESPONSE_PLAN.md）。
    // 注入的就是这段 prompt_text，不再客户端拍平——保留信任带（grounded/raw/rollup）供 agent 推理。
    const resp = await client.query(
      {
        query: queryText,
        agent_id: cfg.agentId,
        project_id: cfg.projectId,
        // The automatic per-turn recall: the server runs its speed tier within budget_ms. An
        // older server knows this call by recall_gate=true instead (sent as before).
        purpose: "auto",
        budget_ms: budgetMs,
        // Turns on the server-side RecallGate. The field doubles as the marker for "this is
        // the automatic per-turn injection": the server defaults it off and only this call
        // site sets it, so explicit memory_recall tool calls and eval drivers always retrieve.
        recall_gate: cfg.recallGate,
        top_k: cfg.recallTopK,
        render: "full",
      },
      { deadlineMs: hookBudget.deadlineMs, meta, ...(signal ? { signal } : {}) },
    );
    budgets.fromResponse(resp.execution);
    const ex = resp.execution;
    const server = ex ? `${ex.outcome ?? "?"}/${ex.tier ?? "?"}/${ex.retrieval_mode ?? "?"}` : "none";
    const grounded = resp.grounded_facts ?? [];
    const rawTurns = (resp.raw_context ?? []).reduce((n, s) => n + (s.turns?.length ?? 0), 0);
    const promptText = (resp.prompt_text ?? "").trim();
    const elapsedMs = Date.now() - startedMs;
    const base = { budgetMs, server, ...(meta.bytes !== undefined ? { responseBytes: meta.bytes } : {}) };

    const pb = resp.prompt_blocks;
    const blocksUsable = pb !== undefined && pb.protocol === BLOCKS_PROTOCOL && Array.isArray(pb.blocks);
    const gate = passesInjectionGate(resp, cfg.recallMinScore);
    logger.info(
      `[${PLUGIN_ID}] auto_recall grounded=${grounded.length} rawTurns=${rawTurns} minScore=${cfg.recallMinScore} ` +
      `promptTextLen=${promptText.length} server=${server} gate=${gate.pass ? "pass" : "fail"} (${gate.why})`,
    );

    // 门槛：无渲染内容，或（块可用时）既无达标的事实/话题摘要，也无未抽取原文块、会话摘要块 → 不注入
    // （块不可用时交给下面的 composeRecall 报协议错误）
    if (!promptText || (blocksUsable && !gate.pass)) {
      logger.info(
        `[${PLUGIN_ID}] auto_recall no memories to inject (${promptText ? gate.why : "empty prompt_text"}) elapsedMs=${elapsedMs}`,
      );
      return { report: { outcome: "not_injected", why: promptText ? "below the gate" : "empty", ...base } };
    }

    let shown: BridgeShown | null = null;
    try {
      shown = params.bridgeShown ? await params.bridgeShown() : null;
    } catch {
      shown = null;                    // no bridge this turn: nothing to leave out
    }
    const composed = composeRecall(resp, shown, cfg.recallMinScore, params.seen);
    if (!composed) {
      logger.warn(
        `[${PLUGIN_ID}] auto_recall not injected: prompt_text came without usable prompt_blocks ` +
        `(protocol=${resp.prompt_blocks?.protocol ?? "none"}, expected ${BLOCKS_PROTOCOL}) elapsedMs=${elapsedMs}`,
      );
      return { report: { outcome: "bad_blocks", ...base } };
    }
    const idIndex = composed.idIndex;
    const rawIndex = composed.rawIndex;
    const relatedSkills = resp.related_skills ?? [];
    const skillsBlock = buildHeliconSkillsBlock(relatedSkills);
    const prependContext = wrapRelevantMemoriesBlock(
      composed.text + idIndex.text + rawIndex.text + skillsBlock.text,
    );

    logger.info(
      `[${PLUGIN_ID}] auto_recall injected organized memory (grounded=${grounded.length} rawTurns=${rawTurns} idIndex=${idIndex.count} rawRefs=${rawIndex.count} skillsBlock=${skillsBlock.count} shownInBridge=${composed.removed} keptMore=${composed.keptMore} otherVersion=${composed.otherVersion} repeated=${composed.repeated}) elapsedMs=${elapsedMs}`,
    );
    if (cfg.debug) {
      // The injected block is otherwise invisible: llm_input logs only the user's own
      // prompt, and the session JSONL does not persist prepended context either — so
      // without this there is no way to tell what the model actually received, and
      // "the model said it has no such memory" cannot be told apart from "the memory
      // was never injected". Newlines folded to ↵ so one turn stays on one log line.
      // debug is optional on ClientLogger; fall back to info so the block is still
      // observable when a caller supplies a logger without one.
      (logger.debug ?? logger.info)(
        `[${PLUGIN_ID}] auto_recall injected block: ${prependContext.replace(/\n/g, "↵")}`,
      );
    }

    return {
      prependContext,
      shown: composed.injected,
      report: {
        outcome: "injected", ...base,
        // (content blocks shown whole: not the legend or headers, not those left to the bridge)
        blocks: (pb?.blocks ?? []).filter((b) => b.ref.type !== "legend" && b.ref.type !== "header").length
          - composed.removed - composed.repeated,
        chars: prependContext.length,
      },
    };
  } catch (err) {
    const elapsedMs = Date.now() - startedMs;
    const kind = heliconErrorKind(err);
    logger.warn(
      `[${PLUGIN_ID}] hook=before_prompt_build → recall FAILED (${kind}) elapsedMs=${elapsedMs} ` +
      `budgetMs=${budgetMs} err=${String(err)}`,
    );
    return { report: { outcome: "failed", errorKind: kind, budgetMs } };
  }
}
