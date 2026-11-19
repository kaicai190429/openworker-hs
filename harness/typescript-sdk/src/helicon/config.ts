/**
 * helicon-memory plugin config schema and runtime parser.
 *
 * Hand-rolled validation mirrors the style used by memory-lancedb's config.ts —
 * keeps error messages explicit and avoids pulling typebox runtime into the
 * config layer. Tool parameter schemas in index.ts still use typebox.
 */

export const MEMORY_CATEGORIES = [
  "preference",
  "fact",
  "decision",
  "entity",
  "instruction",
  "episode",
  "pattern",
  "other",
] as const;
export type MemoryCategory = (typeof MEMORY_CATEGORIES)[number];

export const CAPTURE_MODES = ["direct", "operator"] as const;
export type CaptureMode = (typeof CAPTURE_MODES)[number];

export const DEFAULT_BASE_URL = "http://127.0.0.1:5001";
export const DEFAULT_AGENT_ID = "openworker";
export const DEFAULT_PROJECT_ID = "";
export const DEFAULT_TOP_K = 5;
export const DEFAULT_TIMEOUT_MS = 10_000;
export const DEFAULT_CAPTURE_MAX_CHARS = 500;
export const DEFAULT_RECALL_MAX_CHARS = 1000;
// A floor, not a relevance cut: hs's recall scores are fused and reranked (observed 0.8-1.3 on
// an agent instance), so 0.01 drops only what scored nothing at all.
export const DEFAULT_RECALL_MIN_SCORE = 0.01;
// The per-turn auto-recall (was 3: that starved list/aggregate recall once llm_rerank stopped
// padding the result to keep_n). The memory_recall tool and CLI search use topK instead.
export const DEFAULT_RECALL_TOP_K = 12;
export const DEFAULT_CAPTURE_MODE: CaptureMode = "operator";

/**
 * Bearer token sent when no credential is configured.
 *
 * The plugin ALWAYS sends an Authorization header; an unconfigured deployment
 * sends this placeholder rather than nothing.  Two reasons:
 *
 *  1. A middlebox that substitutes the real per-user token (IT / ms3-security)
 *     needs a header to REPLACE.  If the plugin refused to start without a
 *     credential there would be no request to intercept at all, and that whole
 *     deployment model could not work.
 *  2. "No header" and "a header the server rejects" already land in the same
 *     place — memory answers 401, multimedia falls back to the public pool — so
 *     sending the placeholder changes no behaviour and makes the intent explicit.
 *
 * Deliberately NOT `hs_`-prefixed.  Real credentials are `hs_` + 43 urlsafe
 * chars and are minted server-side only (the register API takes a username,
 * never a caller-supplied token), so this string can never become valid.
 * Staying out of that namespace also keeps it out of the server's log-redaction
 * rule, which masks `hs_…` down to `hs_***abcd`: this value is not a secret and
 * is far more useful printed in full.
 */
export const UNCONFIGURED_API_KEY = "openclaw-unconfigured-tenant";

// The agent's own writes (memory_store): off — see memoryStoreTool below
export const DEFAULT_MEMORY_STORE_TOOL = false;

// Media (multimedia search) defaults
export const DEFAULT_AUTO_MEDIA = true;
export const DEFAULT_MEDIA_ADMIN_TOOLS = false;
export const DEFAULT_MEDIA_MAX_RESULTS = 5;
export const DEFAULT_MEDIA_SEARCH_MODE = 2;   // 2 = VLM + CLIP hybrid
export const DEFAULT_MEDIA_KB_ID = "";
export const DEFAULT_MEDIA_RERANK_MODEL = "BGE";
// Minimum similarity to include a result. Filters irrelevant "long-tail" matches.
// VLM+CLIP+BGE reranker scores: strong match ~0.5+, weak ~0.05-0.15, noise ~0.01-0.04.
export const DEFAULT_MEDIA_MIN_SIMILARITY = 0.05;

// SessionBridge defaults (P3)
//
// Budget rationale (targeting ~90% scenario coverage):
//
//   Episodic (recent sessions):
//     7 sessions × ~600 chars/summary (complex sessions run 500-2000 chars,
//     1200 snippet limit captures ~90% of real summaries) ≈ 4200 chars ≈ 2100 tokens
//     → budget: 40% of total
//
//   Pinned semantics (important long-term facts):
//     20 facts × ~150 chars/fact ≈ 3000 chars ≈ 1500 tokens
//     top_k=20 covers power users with 50-100+ memories (top 15 was insufficient)
//     → budget: up to 80% cumulative
//
//   Procedures (workflow steps):
//     In practice, Operator rarely classifies memories as "procedural"; most
//     workflow info lives in semantic memories.  Keep a small limit (5) to
//     capture explicit procedural records when they exist, without wasting budget.
//     → budget: remainder
//
//   Total: 2500 tokens
//     = 1.25% of Claude Sonnet 4.6 (200k ctx) — negligible
//     = 7.8%  of a 32k-ctx model — acceptable (injected once, prompt-cached)
//
//   char/token ratio: 2.0 (conservative for mixed Chinese/English tech content)
export const DEFAULT_SESSION_BRIDGE = true;
export const DEFAULT_SESSION_BRIDGE_EPISODIC_LIMIT = 7;
export const DEFAULT_SESSION_BRIDGE_PINNED_MIN_IMPORTANCE = 0.85;
export const DEFAULT_SESSION_BRIDGE_PINNED_MIN_RECALL = 5;
export const DEFAULT_SESSION_BRIDGE_PINNED_TOP_K = 20;
export const DEFAULT_SESSION_BRIDGE_PROCEDURE_LIMIT = 5;
export const DEFAULT_SESSION_BRIDGE_TOKEN_BUDGET = 2500;

export type MemoryConfig = {
  baseUrl: string;
  agentId: string;
  projectId: string;
  topK: number;
  timeoutMs: number;
  autoRecall: boolean;
  autoCapture: boolean;
  /** whether the agent gets the memory_store tool (writes of its own, beside what capture extracts) */
  memoryStoreTool: boolean;
  captureMode: CaptureMode;
  captureMaxChars: number;
  storeGate: boolean;
  recallGate: boolean;
  recallMaxChars: number;
  recallMinScore: number;
  recallTopK: number;
  customTriggers?: string[];
  apiKey: string;  // always set; UNCONFIGURED_API_KEY when none configured
  debug: boolean;
  // Session transcript storage (P1)
  syncTranscript: boolean;
  summaryOnSessionEnd: boolean;
  transcriptMaxCharsPerTurn: number;
  // Multimedia media search/upload (autoMedia)
  autoMedia: boolean;
  mediaAdminTools: boolean;
  mediaMaxResults: number;
  mediaSearchMode: number;
  mediaKbId: string;
  mediaRerankModel: string;
  mediaMinSimilarity: number;
  // SessionBridge (P3) — static system-prompt context injection at session start
  sessionBridge: boolean;                  // enable/disable bridge injection
  sessionBridgeEpisodicLimit: number;      // recent episodic summaries to include (1-20)
  sessionBridgePinnedMinImportance: number;// semantic importance threshold for pinned (0-1)
  sessionBridgePinnedMinRecall: number;    // recall_count threshold for high-freq supplement (1-100)
  sessionBridgePinnedTopK: number;         // max pinned semantics after dedup (1-50)
  sessionBridgeProcedureLimit: number;     // max procedural memories (0-20)
  sessionBridgeTokenBudget: number;        // approximate token cap for bridge text (100-8000)
  /** how long a turn may wait for memory before it goes on without (recall + bridge), ms */
  hookTimeoutMs: number;
  /** agents get the Helicon memory tools instead of the engine's built-in memory_* tools */
  replaceBuiltinMemory: boolean;
  /** how long closing a session waits for its fact extraction to finish, ms (0: do not wait) */
  closeWaitMs: number;
  /** shutting the harness down ends the sessions this process captured (HeliconSearch then summarises them) */
  endOnShutdown: boolean;
  /** a session with no turn for this long (ms) is ended too, and one left open by a run that died is ended at the next start; 0: never */
  idleEndMs: number;
  /** media_search also shows the best image hit to the model (one JPEG/PNG; needs a vision model) */
  mediaInlineImages: boolean;
  /** only agents of these names get memory; absent: every agent of the harness */
  agents?: string[];
};

const ALLOWED_KEYS = [
  "baseUrl",
  "agentId",
  "projectId",
  "topK",
  "timeoutMs",
  "autoRecall",
  "autoCapture",
  "memoryStoreTool",
  "captureMode",
  "captureMaxChars",
  "storeGate",
  "recallGate",
  "recallMaxChars",
  "recallMinScore",
  "recallTopK",
  "customTriggers",
  "apiKey",
  "debug",
  "syncTranscript",
  "summaryOnSessionEnd",
  "transcriptMaxCharsPerTurn",
  "autoMedia",
  "mediaAdminTools",
  "mediaMaxResults",
  "mediaSearchMode",
  "mediaKbId",
  "mediaRerankModel",
  "mediaMinSimilarity",
  "sessionBridge",
  "sessionBridgeEpisodicLimit",
  "sessionBridgePinnedMinImportance",
  "sessionBridgePinnedMinRecall",
  "sessionBridgePinnedTopK",
  "sessionBridgeProcedureLimit",
  "sessionBridgeTokenBudget",
  "hookTimeoutMs",
  "replaceBuiltinMemory",
  "closeWaitMs",
  "endOnShutdown",
  "idleEndMs",
  "agents",
  "mediaInlineImages",
] as const;

function assertAllowedKeys(value: Record<string, unknown>): void {
  const unknown = Object.keys(value).filter(
    (key) => !(ALLOWED_KEYS as readonly string[]).includes(key),
  );
  if (unknown.length > 0) {
    throw new Error(`helicon-memory config has unknown keys: ${unknown.join(", ")}`);
  }
}

function resolveEnvVars(value: string): string {
  return value.replace(/\$\{([^}]+)\}/g, (_, envVar: string) => {
    const envValue = process.env[envVar];
    if (envValue === undefined) {
      throw new Error(`Environment variable ${envVar} is not set`);
    }
    return envValue;
  });
}

function resolveApiKey(raw: unknown): string {
  // Never throws and never yields "": the Authorization header is unconditional
  // (see UNCONFIGURED_API_KEY).  A `${VAR}` placeholder whose variable is unset
  // or empty is NOT a configuration error — it is the normal state of a
  // deployment whose credential is injected downstream.  Contrast baseUrl,
  // which keeps throwing on a missing variable: there is no sane fallback for
  // "which server", while there is one for "which tenant".
  if (typeof raw !== "string" || !raw.trim()) {
    return UNCONFIGURED_API_KEY;
  }
  let resolved: string;
  try {
    resolved = resolveEnvVars(raw);
  } catch {
    return UNCONFIGURED_API_KEY;
  }
  return resolved.trim() || UNCONFIGURED_API_KEY;
}

function normalizeBaseUrl(raw: string): string {
  const resolved = resolveEnvVars(raw).trim();
  if (!resolved) {
    throw new Error("baseUrl must not be empty");
  }
  try {
    const url = new URL(resolved);
    return `${url.origin}${url.pathname.replace(/\/+$/, "")}`;
  } catch {
    throw new Error(`baseUrl is not a valid URL: ${resolved}`);
  }
}

function parseIntInRange(
  raw: unknown,
  label: string,
  min: number,
  max: number,
  fallback: number,
): number {
  if (raw === undefined) {
    return fallback;
  }
  if (typeof raw !== "number" || !Number.isFinite(raw)) {
    throw new Error(`${label} must be a finite number`);
  }
  const value = Math.floor(raw);
  if (value < min || value > max) {
    throw new Error(`${label} must be between ${min} and ${max}`);
  }
  return value;
}

function parseFloatInRange(
  raw: unknown,
  label: string,
  min: number,
  max: number,
  fallback: number,
): number {
  if (raw === undefined) {
    return fallback;
  }
  if (typeof raw !== "number" || !Number.isFinite(raw)) {
    throw new Error(`${label} must be a finite number`);
  }
  if (raw < min || raw > max) {
    throw new Error(`${label} must be between ${min} and ${max}`);
  }
  return raw;
}

function parseBoolean(raw: unknown, fallback: boolean): boolean {
  if (raw === undefined) {
    return fallback;
  }
  if (typeof raw !== "boolean") {
    throw new Error(`expected boolean, got ${typeof raw}`);
  }
  return raw;
}

function parseAgentNames(raw: unknown): string[] | undefined {
  if (raw === undefined) return undefined;
  if (!Array.isArray(raw) || raw.some((n) => typeof n !== "string" || !n.trim() || n.length > 100)) {
    throw new Error("agents must be an array of agent names (non-empty strings)");
  }
  if (raw.length > 100) throw new Error("agents must list at most 100 names");
  return raw.map((n: string) => n.trim());
}

function parseCustomTriggers(raw: unknown): string[] | undefined {
  if (raw === undefined) {
    return undefined;
  }
  if (!Array.isArray(raw)) {
    throw new Error("customTriggers must be an array of strings");
  }
  const out = raw.map((trigger, index) => {
    if (typeof trigger !== "string") {
      throw new Error(`customTriggers.${index} must be a string`);
    }
    const normalized = trigger.trim();
    if (!normalized) {
      throw new Error(`customTriggers.${index} must not be empty`);
    }
    if (normalized.length > 100) {
      throw new Error(`customTriggers.${index} must be at most 100 characters`);
    }
    return normalized;
  });
  if (out.length > 50) {
    throw new Error("customTriggers must include at most 50 entries");
  }
  return out;
}

function parseCaptureMode(raw: unknown): CaptureMode {
  if (raw === undefined) {
    return DEFAULT_CAPTURE_MODE;
  }
  if (typeof raw !== "string" || !(CAPTURE_MODES as readonly string[]).includes(raw)) {
    throw new Error(`captureMode must be one of: ${CAPTURE_MODES.join(", ")}`);
  }
  return raw as CaptureMode;
}

export const memoryConfigSchema = {
  parse(value: unknown): MemoryConfig {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      throw new Error("helicon-memory config required");
    }
    const cfg = value as Record<string, unknown>;
    assertAllowedKeys(cfg);

    if (typeof cfg.baseUrl !== "string" || !cfg.baseUrl.trim()) {
      throw new Error("baseUrl is required");
    }
    const baseUrl = normalizeBaseUrl(cfg.baseUrl);

    for (const key of ["agentId", "projectId"] as const) {
      if (cfg[key] !== undefined && cfg[key] !== null && typeof cfg[key] !== "string") {
        throw new Error(`${key} must be a string`);
      }
    }
    const agentId =
      typeof cfg.agentId === "string" && cfg.agentId.trim()
        ? resolveEnvVars(cfg.agentId).trim()
        : DEFAULT_AGENT_ID;
    const projectId =
      typeof cfg.projectId === "string" ? resolveEnvVars(cfg.projectId).trim() : DEFAULT_PROJECT_ID;

    const topK = parseIntInRange(cfg.topK, "topK", 1, 50, DEFAULT_TOP_K);
    const timeoutMs = parseIntInRange(
      cfg.timeoutMs,
      "timeoutMs",
      1000,
      120_000,
      DEFAULT_TIMEOUT_MS,
    );
    const captureMaxChars = parseIntInRange(
      cfg.captureMaxChars,
      "captureMaxChars",
      100,
      10_000,
      DEFAULT_CAPTURE_MAX_CHARS,
    );
    const recallMaxChars = parseIntInRange(
      cfg.recallMaxChars,
      "recallMaxChars",
      100,
      10_000,
      DEFAULT_RECALL_MAX_CHARS,
    );
    const recallMinScore = parseFloatInRange(
      cfg.recallMinScore,
      "recallMinScore",
      0,
      1,
      DEFAULT_RECALL_MIN_SCORE,
    );
    const recallTopK = parseIntInRange(cfg.recallTopK, "recallTopK", 1, 20, DEFAULT_RECALL_TOP_K);

    const autoRecall = parseBoolean(cfg.autoRecall, true);
    const autoCapture = parseBoolean(cfg.autoCapture, true);
    // memory_store：agent 自己往 HeliconSearch 写记忆的工具，默认不注册。它写的记忆不经抽取链路的
    // 来源门禁、不带来源轮次，和 capture 从用户原话抽出来的同一件事各存一份 —— 用户让 agent
    // 忘掉时往往只删掉其中一份，另一份照样被召回；也找不到用户原话，归档里的这件事遗忘不到。
    // 打开（true）恢复这个工具。
    const memoryStoreTool = parseBoolean(cfg.memoryStoreTool, DEFAULT_MEMORY_STORE_TOOL);
    const debug = parseBoolean(cfg.debug, false);
    const captureMode = parseCaptureMode(cfg.captureMode);
    // 服务端的 StoreGate（heliconsearch services/memory/store_gate.py）默认关，由这里显式
    // 打开：抽取前判一次「这段里用户有没有引入值得长期记住的新信息」，没有就整段不抽。
    // 默认 true —— 闸门要挡的正是 agent 会话这一侧的噪声（ping / 心跳轮询 / 纯回忆提问），
    // 而直连 /v1/memory/add 的 benchmark 驱动与后台补抽不传这个字段，因此不受影响。
    // 关掉它（storeGate=false）会让 operator capture 退回「发什么就抽什么」，用于跑分复现。
    // 只对 captureMode=operator 有意义：direct 模式走 /v1/memory/store，根本不过抽取链路。
    const storeGate = parseBoolean(cfg.storeGate, true);
    // Same shape as storeGate, for the read path: ON here and OFF on the server, so the
    // request field is what turns the gate on and only autoRecall is ever judged. Explicit
    // memory_recall tool calls, benchmark drivers and direct scripts send nothing. On the
    // read path that is the ONLY workable discriminator -- eval drivers send render:"full"
    // exactly like autoRecall does, so nothing else in the request tells the two apart.
    const recallGate = parseBoolean(cfg.recallGate, true);
    const customTriggers = parseCustomTriggers(cfg.customTriggers);
    const agents = parseAgentNames(cfg.agents);
    const apiKey = resolveApiKey(cfg.apiKey);

    // P1 transcript fields
    const syncTranscript = parseBoolean(cfg.syncTranscript, true);
    const summaryOnSessionEnd = parseBoolean(cfg.summaryOnSessionEnd, true);
    const transcriptMaxCharsPerTurn = parseIntInRange(
      cfg.transcriptMaxCharsPerTurn,
      "transcriptMaxCharsPerTurn",
      100,
      50_000,
      20_000,
    );

    // Media tools fields
    const autoMedia = parseBoolean(cfg.autoMedia, DEFAULT_AUTO_MEDIA);
    const mediaInlineImages = parseBoolean(cfg.mediaInlineImages, false);
    const mediaAdminTools = parseBoolean(cfg.mediaAdminTools, DEFAULT_MEDIA_ADMIN_TOOLS);
    const mediaMaxResults = parseIntInRange(
      cfg.mediaMaxResults,
      "mediaMaxResults",
      1,
      100,
      DEFAULT_MEDIA_MAX_RESULTS,
    );
    const mediaSearchMode = parseIntInRange(
      cfg.mediaSearchMode,
      "mediaSearchMode",
      0,
      3,
      DEFAULT_MEDIA_SEARCH_MODE,
    );
    const mediaKbId =
      typeof cfg.mediaKbId === "string" ? cfg.mediaKbId.trim() : DEFAULT_MEDIA_KB_ID;
    const mediaRerankModel =
      typeof cfg.mediaRerankModel === "string" && cfg.mediaRerankModel.trim()
        ? cfg.mediaRerankModel.trim()
        : DEFAULT_MEDIA_RERANK_MODEL;
    const mediaMinSimilarity = parseFloatInRange(
      cfg.mediaMinSimilarity,
      "mediaMinSimilarity",
      0,
      1,
      DEFAULT_MEDIA_MIN_SIMILARITY,
    );

    // P3 SessionBridge fields
    const sessionBridge = parseBoolean(cfg.sessionBridge, DEFAULT_SESSION_BRIDGE);
    const sessionBridgeEpisodicLimit = parseIntInRange(
      cfg.sessionBridgeEpisodicLimit,
      "sessionBridgeEpisodicLimit",
      1, 20, DEFAULT_SESSION_BRIDGE_EPISODIC_LIMIT,
    );
    const sessionBridgePinnedMinImportance = parseFloatInRange(
      cfg.sessionBridgePinnedMinImportance,
      "sessionBridgePinnedMinImportance",
      0, 1, DEFAULT_SESSION_BRIDGE_PINNED_MIN_IMPORTANCE,
    );
    const sessionBridgePinnedMinRecall = parseIntInRange(
      cfg.sessionBridgePinnedMinRecall,
      "sessionBridgePinnedMinRecall",
      1, 100, DEFAULT_SESSION_BRIDGE_PINNED_MIN_RECALL,
    );
    const sessionBridgePinnedTopK = parseIntInRange(
      cfg.sessionBridgePinnedTopK,
      "sessionBridgePinnedTopK",
      1, 50, DEFAULT_SESSION_BRIDGE_PINNED_TOP_K,
    );
    const sessionBridgeProcedureLimit = parseIntInRange(
      cfg.sessionBridgeProcedureLimit,
      "sessionBridgeProcedureLimit",
      0, 20, DEFAULT_SESSION_BRIDGE_PROCEDURE_LIMIT,
    );
    const sessionBridgeTokenBudget = parseIntInRange(
      cfg.sessionBridgeTokenBudget,
      "sessionBridgeTokenBudget",
      100, 8000, DEFAULT_SESSION_BRIDGE_TOKEN_BUDGET,
    );

    const hookTimeoutMs = parseIntInRange(cfg.hookTimeoutMs, "hookTimeoutMs", 1000, 120_000, 15_000);
    const replaceBuiltinMemory = parseBoolean(cfg.replaceBuiltinMemory, true);
    const closeWaitMs = parseIntInRange(cfg.closeWaitMs, "closeWaitMs", 0, 300_000, 0);
    const endOnShutdown = parseBoolean(cfg.endOnShutdown, true);
    // 0 turns the idle ending off; otherwise a session untouched for this long is ended on HeliconSearch
    const idleEndMs = cfg.idleEndMs === 0 ? 0 : parseIntInRange(cfg.idleEndMs, "idleEndMs", 1_000, 86_400_000, 1_800_000);

    return {
      baseUrl,
      agentId,
      projectId,
      topK,
      timeoutMs,
      autoRecall,
      autoCapture,
      memoryStoreTool,
      captureMode,
      captureMaxChars,
      storeGate,
      recallGate,
      recallMaxChars,
      recallMinScore,
      recallTopK,
      ...(customTriggers ? { customTriggers } : {}),
      apiKey,
      debug,
      syncTranscript,
      summaryOnSessionEnd,
      transcriptMaxCharsPerTurn,
      autoMedia,
      mediaAdminTools,
      mediaMaxResults,
      mediaSearchMode,
      mediaKbId,
      mediaRerankModel,
      mediaMinSimilarity,
      sessionBridge,
      sessionBridgeEpisodicLimit,
      sessionBridgePinnedMinImportance,
      sessionBridgePinnedMinRecall,
      sessionBridgePinnedTopK,
      sessionBridgeProcedureLimit,
      sessionBridgeTokenBudget,
      hookTimeoutMs,
      replaceBuiltinMemory,
      closeWaitMs,
      endOnShutdown,
      idleEndMs,
      ...(agents ? { agents } : {}),
      mediaInlineImages,
    };
  },
};
