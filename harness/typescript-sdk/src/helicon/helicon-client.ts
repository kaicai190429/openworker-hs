/**
 * Thin HTTP client wrapping HeliconSearch /v1/memory/* endpoints.
 *
 * Single responsibility: speak HTTP, surface typed results, log every call.
 * Does NOT know about openclaw concepts (hooks, sessions) — kept that way
 * so the client is trivially mockable in unit tests.
 *
 * Heliconsearch wraps every response as { retcode, retmsg, data }; we unwrap
 * and surface HeliconApiError on failure so callers can branch on retcode.
 */

import { executeWithRetry, type TransientRetryConfig } from "./retry.js";
import { UNCONFIGURED_API_KEY } from "./config.js";
import type { SourceRef } from "./source-ref.js";

// ── Types ────────────────────────────────────────────────────────────────────

export type HeliconHealth = {
  // hs /v1/memory/health data = {qdrant, embedding, bm25, overall, recall, …}（admin_routes.py）
  qdrant: boolean;
  embedding: boolean;
  bm25: boolean;
  overall: boolean;
  /** the recall's speed tier and budgets (hs recall_budget.health); absent on an older server */
  recall?: RecallHealth;
};

/** hs recall_budget.health(): the configured budgets, or {error} when they could not be read. */
export type RecallHealth = {
  speed_tier?: string;
  effective_tier?: string;
  auto_tier_budget_ms?: number;
  active_budget_ms?: number;
  config_fingerprint?: string;
  error?: string;
};

/**
 * A recall's "execution" (hs RecallBrain._execution): absent when the server runs no budgets
 * (an older server, a benchmark instance).
 */
export type RecallExecution = {
  request_id?: string | null;
  purpose?: string;
  tier?: string;
  strategy?: string;
  auto_tier_budget_ms?: number;
  active_budget_ms?: number;
  effective_budget_ms?: number | null;
  config_fingerprint?: string;
  elapsed_ms?: number;
  outcome?: string;
  retrieval_mode?: string;
  degraded_stages?: string[];
  soft_overruns?: unknown[];
};

/**
 * Per-call options of the recall path's calls. deadlineMs: an absolute time (Date.now() scale) the
 * call ends by, body read included — it is then sent once (no retry). meta: filled with what was
 * read (bytes).
 */
export type RecallCallOptions = {
  signal?: AbortSignal;
  deadlineMs?: number;
  meta?: { bytes?: number };
};

export type MemoryRecord = {
  id: string;
  text: string;
  memory_type: string;
  category: string;
  scope: string;
  agent_id: string;
  project_id: string;
  session_id?: string | null;
  importance: number;
  confidence: number;
  coherence_score?: number | null;
  lifecycle_state: string;
  extraction_method: string;
  supersedes?: string | null;
  source_refs: SourceRef[];
  source_completeness?: string;
  access_count: number;
  recall_count?: number;
  useful_count: number;
  useless_count?: number;
  unique_query_count?: number;
  confirm_count?: number;
  last_accessed_at?: string | null;
  create_time: number;
  update_time?: number;
  score?: number;
  /** hs services/memory/rendered.py: digest of the text and lifecycle state as returned (bridge.v2). */
  version?: string;
  /** bridge.v2 (pinned facts, procedures): its newest unresolved conflicts, ids into `conflicts`. */
  conflict_ids?: string[];
};

export type StoreRequest = {
  text: string;
  agent_id?: string;
  project_id?: string;
  category?: string;
  scope?: string;
  importance?: number;
  resolution?: string;
  extraction_method?: string;
  confidence?: number;
  supersedes?: string | null;
  ttl_days?: number | null;
  source_refs?: SourceRef[];
  topic?: string;
};

export type StoreResult = {
  id: string;
  status: "created" | "dedupe" | "error";
};

export type AddRequest = {
  messages: Array<{
    role: string;
    content: string;
    /**
     * Absolute index of this message in the agent_end message array — the same value the
     * archival path stamps as session_turn.turn_index. Lets heliconsearch anchor extracted
     * facts to a real turn without timestamp matching, and marks this session's raw windows
     * as extracted up to max(turn_index) once extraction completes. Omitted when the caller
     * cannot supply a real index (NEVER the position inside this array — the archival and
     * extraction filters differ, so the two do not correspond).
     */
    turn_index?: number;
    /**
     * ISO-8601 timestamp of the originating message, identical to the value this same message
     * carries as session_turn.created_at once archived. The heliconsearch side uses it as a
     * temporary evidence anchor and, after the transcript is persisted, resolves it back to the
     * real turn_index (see hs session/end backfill). Omitted when the message has no timestamp.
     */
    turn_created_at?: string;
    /**
     * The archived turn this message is (turn_id + content_hash from the /session/turn
     * receipt). heliconsearch then extracts from the archived text and skips turns it has
     * already processed, so a resend does not extract the same words again. A ref that no
     * longer matches the archive leaves the message out of the extraction (reported back in
     * AddResult.archiveMismatched). Omitted when this turn has no confirmed archive entry.
     */
    archive_ref?: ArchiveRef;
  }>;
  agent_id?: string;
  project_id?: string;
  source_refs?: SourceRef[];
  /** session_id used as part of the work_unit_key for dedup on the heliconsearch side. */
  session_id?: string;
  /** Session start datetime string (e.g. "2025-06-01T10:30:00Z") for absolute time
   *  resolution of relative expressions like "last month" → "May 2025" on the hs side. */
  session_datetime?: string;
  /** Server-side StoreGate for this call (heliconsearch services/memory/store_gate.py):
   *  skip extraction entirely when no user turn in this payload introduced anything worth
   *  remembering. The server default is OFF and this is the only caller that asks for it —
   *  benchmark drivers and the background re-extraction post to the same endpoint and must
   *  keep extracting whatever they send. Always sent explicitly, true or false, so the
   *  agent's own config decides and nobody has to remember how the backend was started (the
   *  request level outranks STORE_GATE and the server config). A backend too old to know the
   *  field ignores it. */
  store_gate?: boolean;
};

export type OperationRecord = {
  action: "ADD" | "UPDATE" | "DELETE" | "NOOP" | "ERROR";
  memory_id?: string;
  fact?: string;
  reason?: string;
  status?: string;
};

export type AddResult = {
  operations: OperationRecord[];
  /** Every turn in the payload had been processed before — nothing was extracted this time. */
  alreadyProcessed: boolean;
  /** turn_index of messages whose archive_ref did not match the archive (left out). */
  archiveMismatched: number[];
  /**
   * turn_index of messages the server left out because they are not in the session's archive
   * (archive first, then extract): send them again once they are archived.
   */
  notArchived?: number[];
  /** turn_index of messages whose index holds another text in the archive (left out). */
  archiveConflicts?: number[];
  /**
   * How the server settled this payload: extracted / no_user_turn / gate_skipped /
   * already_processed, or null when extraction did not complete (the server's background
   * re-extraction picks it up). Undefined against a server that does not report it.
   */
  processingState?: string | null;
  /**
   * The server took the payload over durably but has not processed it yet (202 queued=true: the
   * session's earlier work is still running, or these turns are already held by an earlier
   * request). Its ingest worker finishes it — nothing is to be sent again.
   */
  queued?: boolean;
  /** The server's ingest work unit (GET /v1/memory/ingest/work-units/<id>). */
  workUnitId?: string | null;
};

export type QueryRequest = {
  /** Server-side RecallGate: skip retrieval when the turn needs no long-term memory. */
  recall_gate?: boolean;
  /** "auto": the per-turn automatic recall (its speed tier); "active": a search asked for. */
  purpose?: "auto" | "active";
  /** what the caller has left for the recall, ms: the server only shortens its budget to it */
  budget_ms?: number;
  query: string;
  agent_id?: string;
  project_id?: string;
  top_k?: number;
  context_messages?: Array<{ role: string; content: string }>;
  current_session_id?: string;
  max_rounds?: number;
  include_details?: boolean;
  return_audit?: boolean;
  // 渲染档：缺省=服务端默认(compact)；none=只结构不渲染；full=穷尽；compact=精简注入。
  render?: "none" | "full" | "compact";
};

// ── organized 视图类型（/v1/memory/query 直接返回，见 ORGANIZED_RESPONSE_PLAN.md）──

export type EvidenceRef = {
  quote: string;
  source: string;
  speaker: string;
};

/**
 * hs 的 memory_id 形如 uuid4().hex —— 32 位 hex。
 *
 * organized 响应里 `grounded_facts[].id` / `topic_rollups[].id` / raw turn 的 id 都**不是**
 * memory_id，而是 hs 每次查询现生成的临时包 id（recall_package.py: `pkg:<12hex>`）：它不落库、
 * 每次查询都变，喂给 /v1/memory/explain 必然 404。可寻址的 id 只存在于 `source_memory_ids`。
 * memory_get / memory_forget 只接受本模式匹配的 id。
 */
export const MEMORY_ID_PATTERN = /^[0-9a-f]{32}$/i;

/**
 * 从 organized 条目里取出第一个可寻址的 memory_id（可直接用于 memory_get / memory_forget）。
 * 按格式校验而非盲取 [0]，避免把上游异常值当 id 透出给模型。返回 null 表示该条目不可展开
 * （rollup / raw turn 天然没有 source_memory_ids）。
 */
export function pickAddressableMemoryId(entry: {
  source_memory_ids?: string[] | null;
}): string | null {
  for (const raw of entry.source_memory_ids ?? []) {
    const id = (raw ?? "").trim().toLowerCase();
    if (MEMORY_ID_PATTERN.test(id)) {
      return id;
    }
  }
  return null;
}

// 派生且证据链可溯源的结论；带逐字 evidence quote + score。
export type GroundedFact = {
  id: string;
  claim: string;
  derived: boolean;
  score: number | null;
  source_memory_ids: string[];
  support_level: string;
  speaker: string;
  evidence: EvidenceRef[];
  session_ids: string[];
  corroborated_by: string[];
  shared_evidence_with: string[];
};

// 写入期话题聚合，答"有哪些/几次/list all"。
export type TopicRollup = {
  id: string;
  topic: string;
  summary: string;
  derived: boolean;
  score: number | null;
  support_level: string;
  aggregates: string[];
  backing_present: boolean;
};

// 逐字原文 turn（ground truth）。
export type RawTurn = {
  id: string;
  turn: string | number | null;
  speaker: string;
  text: string;
  kind: string;
  score: number | null;
  /** a window of turns whose facts have not been extracted yet (the server's RAW_FALLBACK material) */
  is_orphan?: boolean;
};

export type RawContextSession = {
  session_id: string;
  turns: RawTurn[];
};

// 证据链断裂的派生断言，隔离待核实。
export type NeedsVerification = {
  id: string;
  claim: string;
  derived: boolean;
  score: number | null;
  source_memory_ids: string[];
  trace_status: string;
  reason: string;
  possible_support: string[];
};

export type RecallLayers = {
  memory_count: number;
  trace_span_count: number;
  evidence_span_count: number;
  category_count?: number;
  raw_window_count?: number;
  fallback_triggered: boolean;
  fallback_reason: string | null;
  rounds_used: number;
  stop_reason: string;
  reranked?: boolean;
  retrieval_sources?: Record<string, number>;
  contract?: Record<string, unknown>;
};

export type RelatedSkill = {
  skill_id: string;
  name: string;
  description?: string;
  agent_id?: string;
  project_id?: string;
  version?: number;
  score?: number;
  // HS 端仍带此字段（绝对 URL），但 recall.ts 必须丢弃它、不写进 prompt，
  // 避免把 HS host 暴露给模型。SKILL.md 正文由 skill_get 工具进程内通过
  // getSkill(skillId) 拉取。
  location?: string;
};

/**
 * A unit of stored statements the local model recorded as conflicting (hs rendered.conflict_units).
 * prompt_text does not say so: their facts are memory blocks among the FACTS, a unit's together in
 * the order they were said, each with its time. Some were not found by the query itself (found:
 * false) — pulled in for the fact they bear on — so they are in neither grounded_facts nor
 * needs_verification: here is where a memory block of theirs is looked up.
 */
export type ConflictUnit = {
  id: string;
  facts: ConflictFact[];
  relation_ids: string[];
  omitted: number;
};

/** What a block of prompt_text shows (hs recall_render.render_blocks). */
export type PromptBlockRef =
  | { type: "legend" }
  | { type: "header"; section?: string; session_id?: string; gap?: number }
  | { type: "memory"; memory_id: string | null; version: string | null; extras: string[]; unverified: boolean }
  | { type: "topic"; topic_name: string; version: string | null; coverage: string | null; extras: string[] }
  | { type: "session_summary"; session_id: string; memory_id: string; generation: number; version: string; extras: string[] }
  | { type: "raw"; session_id: string; turns: number | string | null; orphan: boolean; label?: string }
  | { type: "skills" };

export type PromptBlock = { text: string; ref: PromptBlockRef };

export type QueryResponse = {
  _legend?: Record<string, unknown>;
  grounded_facts: GroundedFact[];
  topic_rollups: TopicRollup[];
  raw_context: RawContextSession[];
  needs_verification: NeedsVerification[];
  /** Units of facts the local model recorded as conflicting (structured only; see ConflictUnit). */
  conflicts?: ConflictUnit[];
  // 服务端渲染的答题/注入上下文（render!=none 时）。autoRecall 直接注入这段。
  prompt_text?: string;
  /** prompt_text as the blocks it is made of ("blocks.v1"): their texts joined are prompt_text. */
  prompt_blocks?: { protocol: string; blocks: PromptBlock[] };
  recall_layers: RecallLayers;
  audit?: Record<string, unknown>;
  // 已发布、匹配当前 query 的 skill 列表（qdrant-backed skill_search 层注入到 /v1/memory/query）。
  // 顶层字段，不在 data 包装内。客户端用它做结构化注入（<recalled_skills> 块）。
  related_skills?: RelatedSkill[];
  /** the recall's tier, budgets and outcome (absent: the server runs no budgets) */
  execution?: RecallExecution;
};

export type ReasonRequest = {
  query: string;
  agent_id?: string;
  project_id?: string;
  top_k?: number;
};

export type ReasonResult = {
  // hs /v1/memory/reason data = {answer, supporting_memory_ids, confidence, model,
  // finish_reason, context_memories}（memory_routes.py）
  answer: string;
  supporting_memory_ids: string[];
  confidence: number;
  model?: string;
  finish_reason?: string;
  context_memories?: Array<Record<string, unknown>>;
};

export type FeedbackRequest = {
  memory_id: string;
  useful: boolean;
  correction?: string;
};

export type MemoryStats = {
  // hs store.stats() data = {total, by_lifecycle, by_type, by_category}（store.py）
  total: number;
  by_lifecycle: Record<string, number>;
  by_type: Record<string, number>;
  by_category: Record<string, number>;
  agent_id?: string;
};

// Minimal logger shape; deliberately narrow so the client doesn't pull in
// openclaw plugin types and stays trivially mockable.
export type ClientLogger = {
  info: (msg: string) => void;
  warn: (msg: string) => void;
  debug?: (msg: string) => void;
};

export class HeliconApiError extends Error {
  readonly status: number;
  readonly retcode: number | undefined;
  readonly retmsg: string | undefined;
  readonly body: unknown;

  constructor(params: {
    status: number;
    retcode?: number;
    retmsg?: string;
    body?: unknown;
    message: string;
  }) {
    super(params.message);
    this.name = "HeliconApiError";
    this.status = params.status;
    this.retcode = params.retcode;
    this.retmsg = params.retmsg;
    this.body = params.body;
  }
}

/** HeliconSearch says "not found" as HTTP 404, and also in the body (`{"code": 404, ...}` under a 200). */
export function isNotFound(err: unknown): boolean {
  return err instanceof HeliconApiError && (err.status === 404 || err.retcode === 404);
}

/**
 * A call that got no HTTP answer to read: its own deadline passed (`deadline`, before the body was
 * read through, too), the caller's signal aborted it (`aborted`), the connection failed
 * (`network`), or the body was larger than this client reads (`too_large`). An HTTP status is
 * always a HeliconApiError — callers branch on its `status`.
 */
export type TransportErrorKind = "deadline" | "aborted" | "network" | "too_large";

export class HeliconTransportError extends Error {
  readonly kind: TransportErrorKind;
  readonly elapsedMs: number;

  constructor(kind: TransportErrorKind, message: string, elapsedMs: number, cause?: unknown) {
    // (the cause is kept: the SDK's retry reads network error codes from it)
    super(message, cause === undefined ? undefined : { cause });
    this.name = "HeliconTransportError";
    this.kind = kind;
    this.elapsedMs = elapsedMs;
  }
}

/** The kind of a failed call, for logs: a transport kind, "unavailable" (HTTP 502/503/504), "http", or "other". */
export function heliconErrorKind(err: unknown): TransportErrorKind | "unavailable" | "http" | "other" {
  if (err instanceof HeliconTransportError) {
    return err.kind;
  }
  if (err instanceof HeliconApiError) {
    return err.status === 502 || err.status === 503 || err.status === 504 ? "unavailable" : "http";
  }
  return "other";
}

/** A response body larger than this is not read (an error, not a cut-off text parsed anyway). */
export const MAX_RESPONSE_BYTES = 16 * 1024 * 1024;

type BoundedResponse = { status: number; ok: boolean; text: string; bytes: number };

/**
 * One HTTP exchange under one deadline: the timer runs until the body is read through (the SDK's
 * fetchWithTimeout stops it when the headers come, and replaces the caller's signal with its own);
 * the caller's signal aborts it too; the body is read in chunks up to maxBytes.
 */
export async function fetchBounded(
  fetchFn: typeof fetch,
  url: string,
  init: RequestInit,
  timeoutMs: number,
  parent?: AbortSignal,
  maxBytes: number = MAX_RESPONSE_BYTES,
): Promise<BoundedResponse> {
  const started = Date.now();
  const ctl = new AbortController();
  let timedOut = false;
  const budget = Math.max(1, Math.floor(timeoutMs));
  const timer = setTimeout(() => {
    timedOut = true;
    ctl.abort();
  }, budget);
  const onParent = () => ctl.abort();
  if (parent) {
    if (parent.aborted) {
      ctl.abort();
    } else {
      parent.addEventListener("abort", onParent, { once: true });
    }
  }
  const failed = (err: unknown, during: string): HeliconTransportError => {
    const elapsed = Date.now() - started;
    if (timedOut) {
      return new HeliconTransportError("deadline", `request timed out after ${budget}ms (${during})`, elapsed, err);
    }
    if (parent?.aborted) {
      return new HeliconTransportError("aborted", `request aborted by the caller (${during})`, elapsed, err);
    }
    const msg = err instanceof Error ? err.message : String(err);
    // (undici puts the code — ECONNREFUSED, ECONNRESET, … — on the error's cause; named in the
    // message, because the SDK's retry looks for it only one cause deep and this wraps the error)
    const code = [err, (err as { cause?: unknown } | null)?.cause]
      .map((e) => (e && typeof e === "object" ? (e as { code?: unknown }).code : undefined))
      .find((c): c is string => typeof c === "string");
    return new HeliconTransportError(
      "network", `fetch failed: ${msg}${code ? ` [${code}]` : ""} (${during})`, elapsed, err);
  };
  try {
    let res: Response;
    try {
      res = await fetchFn(url, { ...init, signal: ctl.signal });
    } catch (err) {
      throw failed(err, "connecting");
    }
    const chunks: Uint8Array[] = [];
    let bytes = 0;
    if (res.body) {
      const reader = res.body.getReader();
      for (;;) {
        let part: ReadableStreamReadResult<Uint8Array>;
        try {
          part = await reader.read();
        } catch (err) {
          throw failed(err, "reading the body");
        }
        if (part.done) {
          break;
        }
        bytes += part.value.byteLength;
        if (bytes > maxBytes) {
          ctl.abort();
          throw new HeliconTransportError(
            "too_large", `response body over ${maxBytes} bytes, not read`, Date.now() - started);
        }
        chunks.push(part.value);
      }
    }
    const all = new Uint8Array(bytes);
    let at = 0;
    for (const c of chunks) {
      all.set(c, at);
      at += c.byteLength;
    }
    return { status: res.status, ok: res.ok, text: new TextDecoder().decode(all), bytes };
  } finally {
    clearTimeout(timer);
    parent?.removeEventListener("abort", onParent);
  }
}

// ── Session transcript types ──────────────────────────────────────────────────

export type SessionStartRequest = {
  session_id: string;
  agent_id?: string;
  project_id?: string;
  conversation_id?: string;
  channel?: string;
  metadata?: Record<string, unknown>;
  /** 真实会话开始时间（ISO），存为 started_at，供 hs 召回期做相对→绝对时间解析
   *  （见 heliconsearch TEMPORAL_ANCHOR_PLAN.md）。缺省时 hs 退回 utcnow()。 */
  session_datetime?: string;
};

export type SessionStartResult = {
  session_id: string;
  status: string;
  started_at: string;
  conversation_id: string;
};

export type TurnForTranscript = {
  turn_index: number;
  role: string;
  content: string;
  content_type?: string;
  created_at?: string;
  token_count?: number;
  metadata?: Record<string, unknown>;
};

export type AppendTurnsRequest = {
  session_id: string;
  agent_id?: string;
  project_id?: string;
  conversation_id?: string;
  channel?: string;
  turns: TurnForTranscript[];
};

/** An archived turn, as /v1/memory/session/turn confirmed it. */
export type ArchiveRef = { turn_id: string; content_hash: string };

/** One /v1/memory/session/turn receipt entry, in request order. */
export type TurnReceipt = {
  turn_index: number;
  turn_id?: string;
  role?: string;
  content_hash?: string;
  /**
   * stored / replayed: the archive holds exactly this content (turn_id + content_hash are
   * its ArchiveRef). revision_conflict: a different content (or the same text under another
   * role) is archived at this index and was kept (turn_id / content_hash are that one's;
   * incoming_hash is what was sent).
   * error: not written — resending may succeed.
   */
  sql_status: "stored" | "replayed" | "revision_conflict" | "error";
  /**
   * Only for stored / replayed. repair_pending: its raw window was not built this time; hs
   * rebuilds missing windows on the next archive call of the session and at session/end.
   */
  raw_status?: "ready" | "not_indexed_by_policy" | "repair_pending" | "disabled";
  incoming_hash?: string;
  error?: string;
};

export type AppendTurnsResult = {
  // v0.5.1: heliconsearch 侧只做 Markdown 归档，返回 archived 条数
  archived: number;
  stored?: number;
  skipped?: number;
  /** Server field name is `conflicts`. */
  conflicts?: number;
  errors?: number;
  /** HTTP 207: some entry conflicted or failed to write. */
  partial?: boolean;
  /** Archived turns whose raw window was not built this time (hs rebuilds it later). */
  raw_repair_pending?: number;
  /** Per-entry receipts, in the order sent. */
  results: TurnReceipt[];
};

// ── Native archive (protocol turns.v2) ────────────────────────────────────────

/** A message as the archive takes it by its native identity (HS services/session/native_archive.py). */
export type NativeWireTurn = {
  native: { id: string; namespace: string };
  /** when it was first said, epoch ms */
  native_ts: number;
  role: "user" | "assistant" | "tool" | "toolResult" | "system";
  parts: Array<{ type: string; text: string }>;
  metadata?: Record<string, unknown>;
  token_count?: number;
};

export type NativeTurnsRequest = {
  session_id: string;
  agent_id: string;
  project_id?: string;
  /** the adapter the native ids are of ("openclaw", …) */
  adapter: string;
  turns: NativeWireTurn[];
};

/** One turns.v2 receipt, in request order. */
export type NativeTurnReceipt = {
  native_id: string;
  namespace: string;
  message_key?: string;
  role?: string;
  content_hash?: string;
  /**
   * stored / copy (first archived in another session of the scope) / replayed: archived as sent.
   * revision_conflict: another content is archived under this identity and stays (content_hash is
   * that one's, incoming_hash what was sent). purged: said before a purge of the session — final,
   * not archived. error: not written — deliver again.
   */
  sql_status: "stored" | "replayed" | "copy" | "revision_conflict" | "purged" | "error";
  turn_id?: string;
  turn_index?: number;
  copy_of?: string | null;
  same_as?: string;
  incoming_hash?: string;
  purged_at?: number;
  error?: string;
  raw_status?: "ready" | "not_indexed_by_policy" | "repair_pending" | "disabled";
};

export type NativeTurnsResult = {
  protocol: "turns.v2";
  session_id: string;
  stored: number;
  replayed: number;
  copies: number;
  conflicts: number;
  purged: number;
  errors: number;
  partial: boolean;
  results: NativeTurnReceipt[];
};

export type NativeStatusRequest = {
  session_id: string;
  agent_id: string;
  project_id?: string;
  adapter: string;
  /** at most 1000 */
  natives: Array<{ id: string; namespace: string }>;
};

export type NativeStatusResult = {
  /** how the session is archived: by native identity, by the adapter's numbering, or not yet */
  protocol: "turns.v2" | "turns.v1" | null;
  purged_at: number | null;
  messages: Array<{
    native_id: string;
    namespace: string;
    message_key?: string;
    archived: boolean;
    turn_id?: string | null;
    turn_index?: number | null;
    role?: string | null;
    content_hash?: string | null;
    copy_of?: string | null;
  }>;
};

/** Who the credential is, to the server (GET /v1/tenant/whoami). */
export type WhoamiResult = { username: string; level?: string; scopes?: string[] };

/** Per-call options of the native archive calls: the caller retries (no retry here). */
export type NativeCallOptions = { signal?: AbortSignal; timeoutMs?: number };

export type SessionEndRequest = {
  session_id: string;
  agent_id?: string;
  project_id?: string;
  // v0.5.1: adapter 侧生成好的摘要文本，heliconsearch 直接存为 episodic memory
  summary_text?: string;
  // v0.5.1: session anchor，格式 [{ type: "session", ref_id: "{agent_type}/{uuid}" }]
  source_refs?: Array<{ type: string; ref_id: string }>;
  // v0.5.1: 本次会话消息总数
  message_count?: number;
};

export type SessionEndResult = {
  session_id: string;
  status: string;
  memory_id?: string;
  ended_at?: string;
};

/** An archived message as GET /v1/memory/session/turns returns it. */
export type SessionTurn = {
  turn_index: number;
  role: string;
  content: string;
  created_at: string | null;
  /** all of it was forgotten by the user (content is empty) */
  omitted: boolean;
};

export type SessionTurnsPage = {
  session_id: string;
  turns: SessionTurn[];
  /** how many messages the session holds */
  total: number;
  first_turn: number | null;
  last_turn: number | null;
  /** more follow this page (up to toTurn when one was given) */
  more: boolean;
};

export type SourceChain = {
  // hs /v1/memory/source/<id> data（admin_routes.py）
  memory: Record<string, unknown>;
  evidence: Array<Record<string, unknown>>;
  source_links: Array<Record<string, unknown>>;
  sessions: Array<Record<string, unknown>>;
  turns: Array<Record<string, unknown>>;
  derivation_chain: Array<{ memory_id: string; edge_type: string; weight: number; meta?: unknown }>;
  source_refs: Array<{ type: string; ref_id: string }>;
  source_completeness: string;
  trace_available?: boolean;
};

export type ExplainResult = {
  memory: MemoryRecord;
  supersedes_chain: Array<{ memory_id: string; text: string; lifecycle_state: string; create_time?: number }>;
  source_refs: Array<{ type: string; ref_id: string }>;
  source_completeness: string;
};

export type ThinkingHealth = {
  // 成功：hs data = {available, api_url, model}（admin_routes.py）
  available?: boolean;
  api_url?: string;
  model?: string;
  // 失败：thinkingHealth() catch 分支合成 {ok:false, error}
  ok?: boolean;
  error?: string;
};

// ── SessionBridge (P3) ───────────────────────────────────────────────────────

export type CategorySummary = {
  topic_name: string;
  category_hint: string;
  summary: string;
  memory_count: number;
  avg_importance: number;
  last_refreshed_at?: string | null;
  /** The memories the summary was made from. */
  source_memory_ids?: string[];
  /** Digest of topic and text. */
  version: string;
  /**
   * How many of the memories it summarised no longer stand, with a digest of which: an agent
   * instance leaves a summary out until it is made again when any no longer does, so what is
   * returned has retired 0 — the version is what the check compares.
   */
  coverage: { summarized: number; retired: number; version: string } | null;
  /** The newest conflicts recorded on what it summarised, ids into `conflicts`. */
  conflict_ids: string[];
};

export type SessionBridgeRequest = {
  agentId: string;
  projectId?: string;
  episodicLimit?: number;        // default 3
  pinnedMinImportance?: number;  // default 0.85
  pinnedMinRecall?: number;      // default 5
  pinnedTopK?: number;           // default 10
  procedureLimit?: number;       // default 5
};

/**
 * How far a session's current summary reaches (hs services/session/summaries.py): a summary made
 * before the conversation went on is `stale`, with `later_turns` archived since it was made.
 */
export type SessionSummaryInfo = {
  /** The session it is the current summary of (from the summary map, not the memory row). */
  session_id: string;
  generation: number;
  fingerprint?: string | null;
  source: string;
  covered_turns: number;
  later_turns: number;
  stale: boolean;
  session_status?: string;
  session_datetime?: string | null;
};

/** A stored statement of a recorded conflict (hs services/memory/rendered.py _fact). */
export type ConflictFact = {
  memory_id: string;
  claim: string;
  version: string;
  /** When it was last said, as written; null when not known. */
  said_at: string | null;
  /** The same as a number (epoch seconds) to order by: the strings, as written, need not sort. */
  said_ts?: number | null;
  /** When it was first said, when that was another time than the last. */
  first_said_at?: string | null;
  /** When what it says holds, when known (a resolved date); null = unknown. */
  in_effect: string | null;
  evidence: { quote: string; session_id: string; turn_index: number | null } | null;
  found?: boolean;
};

/** A conflict recorded on what a SessionBridge returns: its two statements (as recorded, not as said). */
export type BridgeConflict = { relation_id: string; recorded_at?: number; facts: ConflictFact[] };

/** A pinned fact or procedure as a SessionBridge returns it. */
export type BridgeMemory = MemoryRecord & {
  /** hs services/memory/rendered.py: digest of the text and lifecycle state as returned. */
  version: string;
  /** The newest conflicts recorded on it, ids into `conflicts`. */
  conflict_ids: string[];
};

/** What an adapter shows of a SessionBridge, for POST /v1/memory/session_bridge/check. */
export type BridgeCheckItem =
  | { kind: "memory"; memory_id: string; version: string }
  | { kind: "session_summary"; session_id: string; memory_id: string; generation: number; stale: boolean }
  | { kind: "topic"; topic_name: string; version: string; coverage: string | null }
  | { kind: "conflict"; relation_id: string };

export type BridgeCheckResult = {
  protocol?: string;
  current: boolean;
  stale: { kind: string; key: string; why: string }[];
  checked: number;
};

export type SessionBridgeData = {
  /** "bridge.v2" (session-bridge.ts BRIDGE_PROTOCOL): items carry versions, /session_bridge/check exists. */
  protocol: string;
  /** The current summaries of the scope's latest sessions. */
  recent_episodes: (MemoryRecord & { version: string; session_summary: SessionSummaryInfo })[];
  pinned_semantics: BridgeMemory[];
  procedures: BridgeMemory[];
  /** Category Layer topic summaries; empty when the scope has none yet (pinned_semantics stand in). */
  category_summaries: CategorySummary[];
  /** The conflicts the items name (conflict_ids), with what each side says and when it was said. */
  conflicts: BridgeConflict[];
  /** How many to show — the newest of those named by the items shown (the check counts so). */
  conflict_limit: number;
  is_empty: boolean;
};

// ── Client interface ──────────────────────────────────────────────────────────

export interface HeliconClient {
  readonly baseUrl: string;
  health(opts?: { signal?: AbortSignal }): Promise<HeliconHealth>;
  store(req: StoreRequest, opts?: { signal?: AbortSignal }): Promise<StoreResult>;
  add(req: AddRequest, opts?: { signal?: AbortSignal }): Promise<AddResult>;
  query(req: QueryRequest, opts?: RecallCallOptions): Promise<QueryResponse>;
  reason(req: ReasonRequest, opts?: { signal?: AbortSignal }): Promise<ReasonResult>;
  get(memoryId: string, opts?: { signal?: AbortSignal }): Promise<MemoryRecord | null>;
  /**
   * Fetch the published SKILL.md body for a skill matched by `QueryResponse.related_skills`.
   * Returns `null` on 404 (skill may have been retracted or renamed).
   *
   * Host stays inside this client (cfg.baseUrl) — the recall prompt surfaces only
   * `skill_id`, the skill_get tool calls this method on the model's behalf.
   */
  getSkill(skillId: string, opts?: { signal?: AbortSignal }): Promise<{ content: string } | null>;
  delete(memoryId: string, opts?: { signal?: AbortSignal }): Promise<boolean>;
  feedback(
    req: FeedbackRequest,
    opts?: { signal?: AbortSignal },
  ): Promise<{ memory_id: string; useful: number; correction_memory_id: string | null }>;
  stats(agentId: string, projectId?: string, opts?: { signal?: AbortSignal }): Promise<MemoryStats>;
  explain(memoryId: string, opts?: { signal?: AbortSignal }): Promise<ExplainResult | null>;
  // Session transcript (P1)
  sessionStart(req: SessionStartRequest, opts?: { signal?: AbortSignal }): Promise<SessionStartResult>;
  appendTurns(req: AppendTurnsRequest, opts?: { signal?: AbortSignal }): Promise<AppendTurnsResult>;
  /**
   * Archive turns by their native identity (protocol turns.v2). Not retried here: a caller that
   * keeps what it delivers durably retries it, by what each receipt says.
   */
  appendNativeTurns(req: NativeTurnsRequest, opts?: NativeCallOptions): Promise<NativeTurnsResult>;
  /** What the archive holds of native messages of a session (turns.v2). Not retried here. */
  nativeTurnStatus(req: NativeStatusRequest, opts?: NativeCallOptions): Promise<NativeStatusResult>;
  /** The tenant the credential is (401 without a valid one). Not retried here. */
  whoami(opts?: NativeCallOptions): Promise<WhoamiResult>;
  sessionEnd(req: SessionEndRequest, opts?: { signal?: AbortSignal }): Promise<SessionEndResult>;
  /**
   * How many turns heliconsearch has already archived for this session.
   *
   * Used by the session_end handler to decide whether the whole-session
   * fallback archival is still needed. Turns are normally archived per agent
   * turn; re-archiving at session_end would number them differently (the two
   * hooks carry different payloads and therefore need different extractors),
   * producing two parallel sets of the same content.
   */
  sessionTurnCount(
    sessionId: string,
    opts?: { agentId?: string; signal?: AbortSignal },
  ): Promise<{ turn_count: number; exists: boolean }>;
  /** Trigger heliconsearch ThinkingService fallback summary for a closed session. */
  sessionSummary(req: { session_id: string; agent_id?: string; project_id?: string }, opts?: { signal?: AbortSignal }): Promise<{ session_id: string; status: string; memory_id?: string; source?: string }>;
  /**
   * A stretch of a session's archived messages (GET /v1/memory/session/turns): turn_index
   * fromTurn..toTurn (both inclusive), at most limit (hs: default 40, at most 200), with how many
   * the session holds and its first and last turn_index. What the scope's user had forgotten is
   * left out as hs leaves it out of recall — so the scope must be the caller's.
   */
  getSessionTurns(
    sessionId: string,
    opts: { agentId: string; projectId?: string; fromTurn?: number; toTurn?: number; limit?: number;
            signal?: AbortSignal },
  ): Promise<SessionTurnsPage>;
  getSource(memoryId: string, opts?: { signal?: AbortSignal }): Promise<SourceChain>;
  thinkingHealth(opts?: { signal?: AbortSignal }): Promise<ThinkingHealth>;
  /**
   * Fetch SessionBridge data for the start of a new session.
   * Aggregates recent episodic summaries, pinned semantic memories, and
   * procedural memories via fast SQLite queries (no vector search).
   * Includes adaptive degradation: falls back to top-N when strict thresholds
   * yield no results, so cold-start users always get something useful.
   */
  getSessionBridge(req: SessionBridgeRequest, opts?: RecallCallOptions): Promise<SessionBridgeData>;
  /**
   * Whether what the adapter shows of a SessionBridge still stands (bridge.v2): each memory active
   * in that version, each session summary the current generation, each topic summary unchanged
   * over as much still standing, each conflict unresolved and still among those to show.
   */
  checkSessionBridge(
    req: { agentId: string; projectId?: string; items: BridgeCheckItem[] },
    opts?: RecallCallOptions,
  ): Promise<BridgeCheckResult>;
}

// ── Factory ──────────────────────────────────────────────────────────────────

const PLUGIN_ID = "helicon-memory";
// Retries are only applied to transient errors detected by the SDK
// (network-level failures, 5xx-class responses). 3 total attempts with
// short exponential backoff keeps fast-path latency low while still
// surviving brief heliconsearch restarts.
const RETRY_CONFIG = {
  attempts: 3,
  baseDelayMs: 200,
  maxDelayMs: 2000,
} satisfies TransientRetryConfig;
/**
 * Dedicated timeout for /v1/memory/query (see `query()` below).
 *
 * Measured server-side cost on a healthy backend is 2-4s; the general timeoutMs (10s default)
 * leaves too little headroom once the reranker contends for the GPU with other services.
 */
const RECALL_TIMEOUT_MS = 60_000;

export function createHeliconClient(opts: {
  baseUrl: string;
  apiKey?: string;
  timeoutMs: number;
  logger: ClientLogger;
  debug?: boolean;
  fetchFn?: typeof fetch;
}): HeliconClient {
  const baseUrl = opts.baseUrl.replace(/\/+$/, "");
  const apiKey = opts.apiKey;
  const timeoutMs = opts.timeoutMs;
  const logger = opts.logger;
  const debug = opts.debug ?? false;
  const fetchFn = opts.fetchFn ?? fetch;

  function buildHeaders(): Headers {
    const h = new Headers({
      "content-type": "application/json",
      accept: "application/json",
      "user-agent": "openworker-harness-helicon/0.1",
    });
    if (apiKey) {
      h.set("authorization", `Bearer ${apiKey}`);
    }
    return h;
  }

  function logRequestStart(
    method: string,
    path: string,
    bodyBytes: number,
    signal?: AbortSignal,
    effectiveTimeoutMs: number = timeoutMs,
  ) {
    logger.info(
      `[${PLUGIN_ID}] http ${method} ${path} timeoutMs=${effectiveTimeoutMs} bodyBytes=${bodyBytes}${
        signal ? " signal=on" : ""
      }`,
    );
  }

  function logRequestBody(method: string, path: string, body: unknown) {
    if (!debug || body === undefined) {
      return;
    }
    try {
      const json = typeof body === "string" ? body : JSON.stringify(body);
      logger.info(`[${PLUGIN_ID}] http ${method} ${path} request body=${json.slice(0, 2000)}`);
    } catch {
      // ignore stringify failures
    }
  }

  function logResponse(method: string, path: string, status: number, elapsedMs: number, bytes?: number) {
    logger.info(
      `[${PLUGIN_ID}] http ${method} ${path} → ${status} elapsedMs=${elapsedMs}${bytes !== undefined ? ` bytes=${bytes}` : ""}`,
    );
  }

  function logResponseBody(method: string, path: string, body: unknown) {
    if (!debug) {
      return;
    }
    try {
      const json = JSON.stringify(body);
      logger.info(`[${PLUGIN_ID}] http ${method} ${path} response body=${json.slice(0, 2000)}`);
    } catch {
      // ignore stringify failures
    }
  }

  function logFailure(method: string, path: string, status: number, detail: string) {
    logger.warn(`[${PLUGIN_ID}] http ${method} ${path} FAILED status=${status} detail=${detail}`);
    if (status === 401) {
      // The 401 itself says nothing about WHY, and the two causes need opposite
      // fixes. Say which one this is, every time: the startup warning about a
      // missing credential is long gone from the log by the time someone asks
      // "why doesn't memory work", and this line is the one that is still there.
      logger.warn(
        apiKey === UNCONFIGURED_API_KEY
          ? `[${PLUGIN_ID}] 401 — still sending the placeholder "${UNCONFIGURED_API_KEY}": ` +
              `no credential was ever configured, and nothing substituted one on the way. ` +
              `Set config.apiKey (or the env var it references, e.g. HELICON_TENANT_TOKEN).`
          : `[${PLUGIN_ID}] 401 — the configured credential was rejected (not just missing): ` +
              `it may be revoked, or its tenant disabled. Check with ` +
              `GET /v1/tenant/whoami using this same token.`,
      );
    }
  }

  async function callJson<T>(params: {
    method: "GET" | "POST" | "DELETE";
    path: string;
    body?: unknown;
    signal?: AbortSignal;
    operationStage: "read" | "create";
    /** Per-endpoint timeout override for calls whose server-side cost far exceeds the general timeout. */
    timeoutMsOverride?: number;
    /** Per-endpoint retry cap. 1 = no retry — use for expensive endpoints where a retry storm makes congestion worse. */
    retryAttempts?: number;
    /**
     * The recall path's absolute deadline (Date.now() scale), body read included: the call is sent
     * once — a call that ran out of its time is not sent again (the time it would need is gone).
     */
    deadlineMs?: number;
    /** filled with the bytes read */
    meta?: { bytes?: number };
  }): Promise<T> {
    const url = `${baseUrl}${params.path}`;
    const deadlineMs = params.deadlineMs;
    const effectiveTimeoutMs =
      deadlineMs !== undefined ? Math.max(1, deadlineMs - Date.now()) : (params.timeoutMsOverride ?? timeoutMs);
    const init: RequestInit = {
      method: params.method,
      headers: buildHeaders(),
      ...(params.body !== undefined ? { body: JSON.stringify(params.body) } : {}),
    };
    const bodyBytes = init.body ? (init.body as string).length : 0;
    logRequestStart(params.method, params.path, bodyBytes, params.signal, effectiveTimeoutMs);
    logRequestBody(params.method, params.path, params.body);

    const operation = async (): Promise<T> => {
      const started = Date.now();
      let response: BoundedResponse;
      try {
        // (a retry gets the time the general timeout gives it; a deadline call is not retried)
        response = await fetchBounded(
          fetchFn, url, init,
          deadlineMs !== undefined ? deadlineMs - started : effectiveTimeoutMs, params.signal);
      } catch (err) {
        const elapsed = Date.now() - started;
        const msg = err instanceof Error ? err.message : String(err);
        logFailure(params.method, params.path, 0, `${msg} (elapsedMs=${elapsed})`);
        throw err;
      }
      const elapsedMs = Date.now() - started;
      if (params.meta) {
        params.meta.bytes = response.bytes;
      }
      logResponse(params.method, params.path, response.status, elapsedMs, response.bytes);

      if (!response.ok) {
        let body: unknown;
        const bodyText = response.text;
        try {
          body = bodyText ? JSON.parse(bodyText) : undefined;
        } catch {
          body = bodyText;
        }
        const wrap =
          body && typeof body === "object" && !Array.isArray(body)
            ? (body as Record<string, unknown>)
            : undefined;
        const retcode = typeof wrap?.retcode === "number" ? wrap.retcode : undefined;
        const retmsg = typeof wrap?.retmsg === "string" ? wrap.retmsg : undefined;
        const detail = retmsg ?? bodyText.slice(0, 200) ?? "(no body)";
        logFailure(params.method, params.path, response.status, detail);
        throw new HeliconApiError({
          status: response.status,
          retcode,
          retmsg,
          body,
          message: `helicon-memory ${params.method} ${params.path} failed: HTTP ${response.status} ${detail}`,
        });
      }

      const raw = JSON.parse(response.text) as unknown;
      logResponseBody(params.method, params.path, raw);

      if (raw && typeof raw === "object" && !Array.isArray(raw)) {
        const wrap = raw as Record<string, unknown>;

        // heliconsearch's memory endpoints answer in one of two shapes (both current):
        //   { code, data }  → data           (session, session_bridge, stats, …)
        //   { code, ... }   → all but code    (query: grounded_facts, raw_context, …)
        if ("code" in wrap) {
          const code = wrap.code as number | undefined;
          if (code !== undefined && code >= 400) {
            const msg = (wrap.message as string | undefined) ?? `HTTP ${code}`;
            logFailure(params.method, params.path, response.status, msg);
            throw new HeliconApiError({
              status: response.status,
              retcode: code,
              retmsg: msg,
              body: wrap,
              message: `helicon-memory ${params.method} ${params.path} failed: code=${code} ${msg}`,
            });
          }
          if ("data" in wrap) {
            return wrap.data as T;
          }
          const { code: _code, ...rest } = wrap;
          return rest as T;
        }

        // { retcode, retmsg, data }: what hs's app-wide UnidentifiedTenant handler answers, with
        // HTTP 200, when a write path lets that error through (HeliconSearch_agent.py
        // _handle_unidentified_tenant) — a refusal, not a result.
        if ("retcode" in wrap) {
          const retcode = wrap.retcode as number | undefined;
          if (retcode !== undefined && retcode !== 0) {
            const retmsg = wrap.retmsg as string | undefined;
            logFailure(
              params.method,
              params.path,
              response.status,
              `retcode=${retcode} retmsg=${retmsg ?? "?"}`,
            );
            throw new HeliconApiError({
              status: response.status,
              retcode,
              retmsg,
              body: wrap,
              message: `helicon-memory ${params.method} ${params.path} failed: retcode=${retcode} ${retmsg ?? ""}`,
            });
          }
          return wrap.data as T;
        }
      }
      return raw as T;
    };

    return executeWithRetry({
      operation,
      retry: {
        ...RETRY_CONFIG,
        attempts: deadlineMs !== undefined ? 1 : (params.retryAttempts ?? RETRY_CONFIG.attempts),
        ...(params.signal ? { signal: params.signal } : {}),
      },
    });
  }

  return {
    baseUrl,

    async health(opts) {
      return await callJson<HeliconHealth>({
        method: "GET",
        path: "/v1/memory/health",
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async store(req, opts) {
      // heliconsearch returns {memory_id, action}; normalise to {id, status}
      const raw = await callJson<{ memory_id: string; action: string }>({
        method: "POST",
        path: "/v1/memory/store",
        body: req,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
      return {
        id: raw.memory_id,
        status: (raw.action === "created" ? "created" : "dedupe") as StoreResult["status"],
      };
    },

    async add(req, opts) {
      // Cannot use callJson here for two reasons:
      //   1. heliconsearch returns code=202 (work-unit in flight) which callJson treats
      //      identically to 200 — we need to inspect the code field directly.
      //   2. The Operator calls ThinkingService which can take 60-300s on local LLMs.
      //      We use a dedicated long timeout instead of the general timeoutMs (10-120s).
      const OPERATOR_TIMEOUT_MS = 720_000; // 12 minutes — must exceed server-side THINKING_TIMEOUT_MS(600s)
      const path = "/v1/memory/add";
      const bodyStr = JSON.stringify(req);
      logRequestStart("POST", path, bodyStr.length, opts?.signal);
      logRequestBody("POST", path, req);

      const started = Date.now();
      let response: BoundedResponse;
      try {
        response = await fetchBounded(
          fetchFn,
          `${baseUrl}${path}`,
          { method: "POST", headers: buildHeaders(), body: bodyStr },
          OPERATOR_TIMEOUT_MS,
          opts?.signal,
        );
      } catch (err) {
        const elapsed = Date.now() - started;
        const msg = err instanceof Error ? err.message : String(err);
        logFailure("POST", path, 0, `${msg} (elapsedMs=${elapsed})`);
        throw err;
      }
      const elapsedMs = Date.now() - started;
      logResponse("POST", path, response.status, elapsedMs, response.bytes);

      if (!response.ok) {
        const text = response.text;
        logFailure("POST", path, response.status, text.slice(0, 200));
        throw new HeliconApiError({
          status: response.status,
          message: `helicon-memory POST ${path} failed: HTTP ${response.status} ${text.slice(0, 200)}`,
          body: text,
        });
      }

      const raw = JSON.parse(response.text) as Record<string, unknown>;
      logResponseBody("POST", path, raw);

      // { retcode, retmsg } with HTTP 200 is how HeliconSearch refuses a write it did not accept (a tenant
      // check): not an empty result, or the slice would be taken for extracted and never sent again
      if (raw && !("code" in raw) && typeof raw.retcode === "number" && raw.retcode !== 0) {
        const retmsg = typeof raw.retmsg === "string" ? raw.retmsg : undefined;
        logFailure("POST", path, response.status, `retcode=${raw.retcode} retmsg=${retmsg ?? "?"}`);
        throw new HeliconApiError({
          status: response.status,
          retcode: raw.retcode,
          retmsg,
          body: raw,
          message: `helicon-memory POST ${path} failed: retcode=${raw.retcode} ${retmsg ?? ""}`,
        });
      }

      // (every answer of the server's carries one: `{}`, `[]` or a string is not an acceptance, and the slice must
      // not be taken for extracted and never sent again)
      if (!raw || typeof raw !== "object" || typeof raw.code !== "number") {
        throw new HeliconApiError({
          status: response.status,
          body: raw,
          message: `helicon-memory POST ${path}: the answer has no status code, so it is not known to have been accepted`,
        });
      }
      const code = raw.code;
      const numbers = (v: unknown): number[] =>
        Array.isArray(v) ? (v as unknown[]).filter((x): x is number => typeof x === "number") : [];
      // Messages the server left out of what it did — on every answer, the 202 too: what a work
      // unit took over is the rest of the payload, not these.
      const mismatched = numbers(raw?.archive_mismatched);
      const notArchived = numbers(raw?.not_archived);
      const conflicts = numbers(raw?.archive_conflicts);
      const excluded = {
        archiveMismatched: mismatched,
        ...(notArchived.length > 0 ? { notArchived } : {}),
        ...(conflicts.length > 0 ? { archiveConflicts: conflicts } : {}),
      };

      if (code === 202 && raw?.queued === true && typeof raw?.work_unit_id === "string") {
        // The server's ingest ledger holds these turns durably (its work unit waits behind the
        // session's earlier work, or an earlier request already took them): it finishes them in
        // the background. Accepted — resending would only find them held.
        logger.info(
          `[${PLUGIN_ID}] /v1/memory/add queued as work unit ${raw.work_unit_id} ` +
          `session=${req.session_id ?? "?"} — the server finishes it`,
        );
        return {
          operations: [],
          alreadyProcessed: false,
          ...excluded,
          queued: true,
          workUnitId: raw.work_unit_id,
        };
      }

      if (code === 202) {
        // A 202 that hands over no work unit (the server's 202 is queued + work_unit_id, above)
        // says nothing was taken over: not an acceptance. Taken as "extracted nothing" the caller
        // would advance its capture cursor past turns nobody extracted — surfaced as a retryable
        // error instead, so the slice is sent again on the next turn.
        logger.warn(
          `[${PLUGIN_ID}] /v1/memory/add 202 work-unit already in flight ` +
          `session=${req.session_id ?? "?"} agent=${req.agent_id ?? "?"} — not accepted, will retry`,
        );
        throw new HeliconApiError({
          status: response.status,
          retcode: 202,
          retmsg: "work-unit already in flight",
          body: raw,
          message: `helicon-memory POST ${path} not accepted: code=202 work-unit already in flight`,
        });
      }

      if (code >= 400) {
        const msg = (raw?.message as string | undefined) ?? `code=${code}`;
        logFailure("POST", path, response.status, msg);
        throw new HeliconApiError({
          status: response.status,
          retmsg: msg,
          body: raw,
          message: `helicon-memory POST ${path} failed: code=${code} ${msg}`,
        });
      }

      const data = raw?.data;
      const processing = (raw?.audit as Record<string, unknown> | undefined)?.processing as
        | Record<string, unknown>
        | undefined;
      return {
        operations: Array.isArray(data) ? (data as OperationRecord[]) : [],
        alreadyProcessed: raw?.already_processed === true,
        ...excluded,
        ...(processing && "state" in processing
          ? { processingState: typeof processing.state === "string" ? processing.state : null }
          : {}),
        ...(typeof raw?.work_unit_id === "string" ? { workUnitId: raw.work_unit_id } : {}),
      };
    },

    async query(req, opts) {
      // heliconsearch 直接返回 organized 视图（信任分带）。消费方用 flattenOrganized()
      // 按需拍平（见 helicon-client 类型与 ORGANIZED_RESPONSE_PLAN.md）。
      //
      // 超时/重试单独设定，不用通用 timeoutMs(10s)：
      //  · /v1/memory/query 走完整 RecallBrain（多轮检索 + cross-encoder rerank）。健康时
      //    2-4s，但 rerank 所在 GPU 被其它服务（VLMService 等）抢占时会退化到几十秒。
      //  · 超时抛出的 "request timed out" 会被 SDK 判为 transient，默认重试 3 次 —— 在服务端
      //    已经拥塞时再压两个同样昂贵的请求，只会加剧拥塞。这里显式禁用重试。
      //  · deadlineMs（召回路径：自动召回的 HookBudget、主动检索的预算）给了就按它，含读响应体。
      const data = await callJson<QueryResponse>({
        method: "POST",
        path: "/v1/memory/query",
        body: req,
        operationStage: "read",
        timeoutMsOverride: RECALL_TIMEOUT_MS,
        retryAttempts: 1,
        ...(opts?.signal ? { signal: opts.signal } : {}),
        ...(opts?.deadlineMs !== undefined ? { deadlineMs: opts.deadlineMs } : {}),
        ...(opts?.meta ? { meta: opts.meta } : {}),
      });
      return data;
    },

    async reason(req, opts) {
      return await callJson<ReasonResult>({
        method: "POST",
        path: "/v1/memory/reason",
        body: req,
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async get(memoryId, opts) {
      try {
        return await callJson<MemoryRecord>({
          method: "GET",
          path: `/v1/memory/${encodeURIComponent(memoryId)}`,
          operationStage: "read",
          ...(opts?.signal ? { signal: opts.signal } : {}),
        });
      } catch (err) {
        if (isNotFound(err)) {
          return null;
        }
        throw err;
      }
    },

    async getSkill(skillId, opts) {
      // HS /v1/memory/skill/<id> 返回 data.content = SKILL.md markdown body.
      try {
        const data = await callJson<{ content: string }>({
          method: "GET",
          path: `/v1/memory/skill/${encodeURIComponent(skillId)}`,
          operationStage: "read",
          ...(opts?.signal ? { signal: opts.signal } : {}),
        });
        return { content: data?.content ?? "" };
      } catch (err) {
        if (isNotFound(err)) {
          return null;
        }
        throw err;
      }
    },

    async delete(memoryId, opts) {
      // hs 返回 {code:200, data:{deleted:true}}；not-found → code:404 → callJson 抛错。
      // 之前用 Boolean(data) 对 {deleted:...} 恒为 true（连 deleted:false 也判 true）。
      const data = await callJson<{ deleted?: unknown } | undefined>({
        method: "DELETE",
        path: `/v1/memory/${encodeURIComponent(memoryId)}`,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
      return data?.deleted === true;
    },

    async feedback(req, opts) {
      // hs 返回 data={memory_id, useful:int, correction_memory_id:string|null}
      return await callJson<{ memory_id: string; useful: number; correction_memory_id: string | null }>({
        method: "POST",
        path: "/v1/memory/feedback",
        body: req,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async stats(agentId, projectId, opts) {
      const qs = new URLSearchParams();
      qs.set("agent_id", agentId);
      if (projectId) {
        qs.set("project_id", projectId);
      }
      return await callJson<MemoryStats>({
        method: "GET",
        path: `/v1/memory/stats?${qs.toString()}`,
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async explain(memoryId, opts) {
      try {
        return await callJson<ExplainResult>({
          method: "GET",
          path: `/v1/memory/explain?memory_id=${encodeURIComponent(memoryId)}`,
          operationStage: "read",
          ...(opts?.signal ? { signal: opts.signal } : {}),
        });
      } catch (err) {
        // 404 = 该 id 不存在（已删除/被 supersede/id 无效），是正常业务分支而非故障：
        // 收敛成 null，让调用方给出可操作提示，而不是把一句 HTTP 404 甩给模型。
        if (isNotFound(err)) {
          return null;
        }
        throw err;
      }
    },

    // ── Session transcript (P1) ─────────────────────────────────────────

    async sessionStart(req, opts) {
      return await callJson<SessionStartResult>({
        method: "POST",
        path: "/v1/memory/session/start",
        body: req,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async appendTurns(req, opts) {
      return await callJson<AppendTurnsResult>({
        method: "POST",
        path: "/v1/memory/session/turn",
        body: req,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async appendNativeTurns(req, opts) {
      return await callJson<NativeTurnsResult>({
        method: "POST",
        path: "/v1/memory/session/turn",
        body: { protocol: "turns.v2", ...req },
        operationStage: "create",
        retryAttempts: 1,
        ...(opts?.timeoutMs !== undefined ? { timeoutMsOverride: opts.timeoutMs } : {}),
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async whoami(opts) {
      return await callJson<WhoamiResult>({
        method: "GET",
        path: "/v1/tenant/whoami",
        operationStage: "read",
        retryAttempts: 1,
        ...(opts?.timeoutMs !== undefined ? { timeoutMsOverride: opts.timeoutMs } : {}),
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async nativeTurnStatus(req, opts) {
      return await callJson<NativeStatusResult>({
        method: "POST",
        path: "/v1/memory/session/turn/status",
        body: req,
        operationStage: "read",
        retryAttempts: 1,
        ...(opts?.timeoutMs !== undefined ? { timeoutMsOverride: opts.timeoutMs } : {}),
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async sessionEnd(req, opts) {
      return await callJson<SessionEndResult>({
        method: "POST",
        path: "/v1/memory/session/end",
        body: req,
        operationStage: "create",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async sessionSummary(req, opts) {
      // ThinkingService (qwen3.5-35B) takes 30-60s to generate a summary.
      // Use a dedicated long timeout (180s) and NO retries — LLM generation is
      // not idempotent; retrying creates duplicate episodic memories.
      const SUMMARY_TIMEOUT_MS = 180_000;
      const url = `${baseUrl}/v1/memory/session/summary`;
      const body = JSON.stringify(req);
      const response = await fetchBounded(
        fetchFn,
        url,
        { method: "POST", headers: buildHeaders(), body },
        SUMMARY_TIMEOUT_MS,
        opts?.signal,
      );
      if (!response.ok) {
        const text = response.text;
        throw new HeliconApiError({
          status: response.status,
          message: `helicon-memory POST /v1/memory/session/summary failed: HTTP ${response.status} ${text.slice(0, 200)}`,
          body: text,
        });
      }
      const raw = JSON.parse(response.text) as Record<string, unknown>;
      if ("code" in raw && "data" in raw) {
        return raw.data as { session_id: string; status: string; memory_id?: string; source?: string };
      }
      return raw as { session_id: string; status: string; memory_id?: string; source?: string };
    },

    async sessionTurnCount(sessionId, opts) {
      const qs = new URLSearchParams({ session_id: sessionId });
      if (opts?.agentId) { qs.set("agent_id", opts.agentId); }
      const data = await callJson<{ turn_count?: number; exists?: boolean }>({
        method: "GET",
        path: `/v1/memory/session/turn_count?${qs.toString()}`,
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
      return {
        turn_count: typeof data?.turn_count === "number" ? data.turn_count : 0,
        exists: data?.exists === true,
      };
    },

    async getSessionTurns(sessionId, opts) {
      // session_id as a query parameter: a path segment would run into /session/<path:session_id>
      const qs = new URLSearchParams({ session_id: sessionId, agent_id: opts.agentId });
      if (opts.projectId) { qs.set("project_id", opts.projectId); }
      if (opts.fromTurn !== undefined) { qs.set("from_turn", String(opts.fromTurn)); }
      if (opts.toTurn !== undefined) { qs.set("to_turn", String(opts.toTurn)); }
      if (opts.limit !== undefined) { qs.set("limit", String(opts.limit)); }
      return await callJson<SessionTurnsPage>({
        method: "GET",
        path: `/v1/memory/session/turns?${qs.toString()}`,
        operationStage: "read",
        ...(opts.signal ? { signal: opts.signal } : {}),
      });
    },

    async getSource(memoryId, opts) {
      return await callJson<SourceChain>({
        method: "GET",
        path: `/v1/memory/source/${encodeURIComponent(memoryId)}`,
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
      });
    },

    async thinkingHealth(opts) {
      try {
        return await callJson<ThinkingHealth>({
          method: "GET",
          path: "/v1/memory/thinking/health",
          operationStage: "read",
          ...(opts?.signal ? { signal: opts.signal } : {}),
        });
      } catch (err) {
        // Preserve the actual error so callers can distinguish:
        //   - network/timeout: heliconsearch is unreachable
        //   - HeliconApiError: heliconsearch reachable but ThinkingService failed
        const msg = err instanceof HeliconApiError
          ? `heliconsearch error ${err.status}: ${err.retmsg ?? "unknown"}`
          : err instanceof Error
            ? err.message
            : String(err);
        return { ok: false, error: msg };
      }
    },

    async getSessionBridge(req, opts) {
      const qs = new URLSearchParams({ agent_id: req.agentId });
      if (req.projectId) {
        qs.set("project_id", req.projectId);
      }
      if (req.episodicLimit !== undefined) {
        qs.set("episodic_limit", String(req.episodicLimit));
      }
      if (req.pinnedMinImportance !== undefined) {
        qs.set("pinned_min_importance", String(req.pinnedMinImportance));
      }
      if (req.pinnedMinRecall !== undefined) {
        qs.set("pinned_min_recall", String(req.pinnedMinRecall));
      }
      if (req.pinnedTopK !== undefined) {
        qs.set("pinned_top_k", String(req.pinnedTopK));
      }
      if (req.procedureLimit !== undefined) {
        qs.set("procedure_limit", String(req.procedureLimit));
      }
      return await callJson<SessionBridgeData>({
        method: "GET",
        path: `/v1/memory/session_bridge?${qs.toString()}`,
        operationStage: "read",
        ...(opts?.signal ? { signal: opts.signal } : {}),
        // (on a turn's critical path: sent once, by the turn's deadline)
        ...(opts?.deadlineMs !== undefined ? { deadlineMs: opts.deadlineMs } : {}),
        ...(opts?.meta ? { meta: opts.meta } : {}),
      });
    },

    async checkSessionBridge(req, opts) {
      return await callJson<BridgeCheckResult>({
        method: "POST",
        path: "/v1/memory/session_bridge/check",
        body: { agent_id: req.agentId, project_id: req.projectId ?? "", items: req.items },
        operationStage: "read",
        // asked before every prompt build: a slow answer is treated as no answer (not current)
        retryAttempts: 1,
        ...(opts?.signal ? { signal: opts.signal } : {}),
        ...(opts?.deadlineMs !== undefined ? { deadlineMs: opts.deadlineMs } : {}),
      });
    },
  };
}
