/**
 * The agent-callable memory tools (memory_recall / memory_get / memory_forget / memory_store /
 * skill_get), ported from the OpenClaw plugin. They are registered with the harness through the
 * SDK's own `registerTools` (see index.ts), so the model calls them like any application tool.
 */
import { Type, optionalStringEnum } from "./schema.js";
import { MEMORY_CATEGORIES, type MemoryCategory, type MemoryConfig } from "./config.js";
import {
  HeliconApiError,
  type HeliconClient,
  MEMORY_ID_PATTERN,
  heliconErrorKind,
  pickAddressableMemoryId,
} from "./helicon-client.js";
import {
  DEFAULT_CONTEXT_WINDOW,
  formatPage,
  MAX_CONTEXT_WINDOW,
  parseTurns,
  SESSION_PAGE_SIZE,
  sourceSessions,
  sourceTurns,
  windowAround,
} from "./memory-context.js";
import { formatUnextractedExcerpts, pickUnextractedExcerpts } from "./recall.js";
import type { RecallBudgets } from "./recall-budget.js";
import { makeManualRef } from "./source-ref.js";
import type { ClientLogger } from "./helicon-client.js";

const PLUGIN_ID = "helicon-memory";

/** Memory searches an agent run may make for one user message (the recall is injected already). */
const MAX_SEARCHES_PER_TURN = 1;
/** memory_get calls an agent run may make for one user message. */
const MAX_OPENS_PER_TURN = 3;

export type ToolCtx = { sessionKey?: string; sessionId?: string };
/**
 * What a tool returns. `data`, when present, is what the model gets instead of the joined text (a structured
 * result the application can also read from the journal); `image` adds one JPEG/PNG the model can look at.
 */
export type ToolResult = {
  content: { type: "text"; text: string }[];
  details?: Record<string, unknown>;
  data?: unknown;
  image?: { mimeType: "image/jpeg" | "image/png"; base64: string };
};
export type ToolDef = {
  name: string;
  label?: string;
  description: string;
  parameters: Record<string, unknown>;
  execute(toolCallId: string, args: unknown): Promise<ToolResult>;
};
/** What the adapter offers the ported tool code (the shape of the host API it was written against). */
export type ToolHost = {
  logger: ClientLogger;
  registerTool(tool: ToolDef | ((ctx: ToolCtx) => ToolDef), opts?: { name: string }): void;
};
export type SessionState = { turnUse: Map<string, { searches: number; opens: number }> };

export function formatHeliconError(err: unknown): string {
  if (err instanceof HeliconApiError) {
    return `HeliconApiError status=${err.status} retcode=${err.retcode ?? "?"} retmsg=${err.retmsg ?? "?"}`;
  }
  if (err instanceof Error) {
    return `${err.name}: ${err.message}`;
  }
  return String(err);
}

// ── Tools ────────────────────────────────────────────────────────────────────

type MemoryIdCheck = { ok: true; id: string } | { ok: false; hint: string };

/**
 * 校验模型传来的 memoryId 是否真的可寻址（hs memory_id = 32 位 hex）。
 *
 * 模型极易把 organized 响应里的临时包 id（`pkg:<hex>`）当成 memory id 传进来——那是每次查询
 * 现生成、不落库的引用，打到 /v1/memory/explain 只会得到 404。这里在发 HTTP 之前拦下并给出
 * 可操作的指引，避免把一个无意义的 404 甩回给模型。
 */
function normalizeMemoryId(raw: string | undefined): MemoryIdCheck {
  // hs 存的 id 一律是 uuid4().hex（小写）；模型转述时可能改变大小写，统一小写后再精确匹配。
  const id = (raw ?? "").trim().toLowerCase();
  if (!id) {
    return { ok: false, hint: "memoryId is required." };
  }
  if (id.startsWith("pkg:")) {
    return {
      ok: false,
      hint:
        `"${id}" is a per-query package id, not a memory id — it cannot be looked up. ` +
        `Call memory_recall and use the 32-hex id it reports for that fact.`,
    };
  }
  if (!MEMORY_ID_PATTERN.test(id)) {
    return {
      ok: false,
      hint:
        `"${id}" is not a valid memory id (expected 32 hex characters, e.g. from memory_recall). ` +
        `Call memory_recall to obtain a valid id.`,
    };
  }
  return { ok: true, id };
}

export function registerTools(params: {
  api: ToolHost;
  cfg: MemoryConfig;
  client: HeliconClient;
  sessionState: SessionState;
  /** the server's recall budgets: a search's deadline is its active-retrieval budget */
  recallBudgets: RecallBudgets;
}): void {
  const { api, cfg, client, sessionState, recallBudgets } = params;

  /**
   * Take one of this user message's searches / opens for the run's session: true when one was
   * left (and it is now used), false when the limit is reached. A tool built without a session
   * (CLI, scripts) is never limited.
   */
  const take = (turnKey: string, what: "searches" | "opens"): boolean => {
    if (!turnKey) {
      return true;
    }
    const use = sessionState.turnUse.get(turnKey) ?? { searches: 0, opens: 0 };
    const max = what === "searches" ? MAX_SEARCHES_PER_TURN : MAX_OPENS_PER_TURN;
    if (use[what] >= max) {
      return false;
    }
    use[what] += 1;
    sessionState.turnUse.set(turnKey, use);
    return true;
  };
  const used = (turnKey: string, what: "searches" | "opens"): number =>
    sessionState.turnUse.get(turnKey)?.[what] ?? 0;

  // Factories: the run's session key counts its searches and opens (sessionState.turnUse).
  api.registerTool(
    (toolCtx) => ({
      name: "memory_recall",
      label: "Memory Recall (HeliconSearch)",
      // (The text of a tool must not depend on the configuration: the engine keeps the descriptor of a tool name
      // and refuses a changed one, which would switch the memory off at the next start.)
      description:
        "Search long-term memory. Memories relevant to the user's message may already be shown in <relevant-memories>; use this for what they do not cover. One search per user message: make the query cover what is missing.",
      parameters: Type.Object({
        query: Type.String({ description: "Search query (natural language)" }),
        limit: Type.Optional(
          Type.Number({
            description: "Max results to return (1-20; a default is used when omitted)",
          }),
        ),
      }),
      async execute(_toolCallId: string, raw: unknown) {
        const args = (raw ?? {}) as { query?: string; limit?: number };
        const query = (args.query ?? "").trim();
        const limit =
          typeof args.limit === "number" ? Math.max(1, Math.min(20, args.limit)) : cfg.topK;
        const started = Date.now();
        api.logger.info(
          `[${PLUGIN_ID}] tool=memory_recall query="${query.slice(0, 80)}" limit=${limit} agent=${cfg.agentId}`,
        );

        if (!query) {
          return {
            content: [{ type: "text", text: "memory_recall: query is required" }],
            details: { error: "missing_query" },
          };
        }

        // the same key before_prompt_build clears on each user message
        const turnKey = toolCtx.sessionKey ?? toolCtx.sessionId ?? "";
        if (!take(turnKey, "searches")) {
          const made = used(turnKey, "searches");
          api.logger.info(
            `[${PLUGIN_ID}] tool=memory_recall → not searched: ${made} searches already for this message (session=${turnKey})`,
          );
          return {
            content: [{
              type: "text",
              text:
                `memory_recall: not searched — memory was already searched ${made} times for this message, ` +
                "besides the memories shown with it. Answer with what those gave you; if they did not " +
                "have it, tell the user you have no record of it.",
            }],
            details: { error: "recall_limit", calls: made },
          };
        }

        try {
          // memory_recall 是显式搜索工具：只需结构(不需渲染)，取 grounded_facts（派生且
          // 可溯源的记忆事实）按 recallMinScore 过滤展示。render=none 省去服务端渲染开销。
          // an active retrieval: the server's multi-round flow within its active budget; waited for
          // that long plus the transport margin (recall-budget.ts), not a fixed time
          const resp = await client.query({
            query,
            agent_id: cfg.agentId,
            project_id: cfg.projectId,
            purpose: "active",
            top_k: limit,
            render: "none",
          }, { deadlineMs: recallBudgets.activeDeadlineMs() });
          recallBudgets.fromResponse(resp.execution);
          const filtered = (resp.grounded_facts ?? []).filter(
            (f) => (f.score ?? 0) >= cfg.recallMinScore,
          );
          const elapsedMs = Date.now() - started;
          // 只记录/透出可寻址的 memory_id。f.id 是本次查询现生成的临时包 id（`pkg:<12hex>`），
          // 曾被当作 memory id 截断后透给模型，导致 memory_get 必然 404。
          const ids = filtered.map((f) => pickAddressableMemoryId(f) ?? "-");
          api.logger.info(
            `[${PLUGIN_ID}] tool=memory_recall → memories=${filtered.length} (grounded=${resp.grounded_facts?.length ?? 0}) elapsedMs=${elapsedMs} ids=[${ids.join(",")}]`,
          );

          if (filtered.length === 0) {
            // grounded 为空 ≠ 没有相关记忆：话题摘要(rollup)与逐字原文(raw)是另外两条信任带。
            // 它们没有 source_memory_ids（无法 memory_get 展开），但内容照样能回答问题——
            // 以前直接返回 "No relevant memories found." 把这些结果全丢了。
            const rollups = (resp.topic_rollups ?? [])
              .filter((r) => (r.score ?? 0) >= cfg.recallMinScore)
              .slice(0, limit);
            const rawTurns = (resp.raw_context ?? [])
              .flatMap((s) => (s.turns ?? []).map((t) => ({ sessionId: s.session_id, turn: t })))
              .filter((x) => (x.turn.score ?? 0) >= cfg.recallMinScore)
              .slice(0, limit);

            if (rollups.length === 0 && rawTurns.length === 0) {
              return {
                content: [{ type: "text", text: "No relevant memories found." }],
                details: { count: 0 },
              };
            }

            const sections: string[] = [];
            if (rollups.length > 0) {
              sections.push(
                "Topic summaries (aggregated — no memory id, cannot be expanded):\n" +
                  rollups
                    .map(
                      (r, i) =>
                        `${i + 1}. ${r.topic ? `[${r.topic}] ` : ""}${r.summary} (${((r.score ?? 0) * 100).toFixed(0)}%)`,
                    )
                    .join("\n"),
              );
            }
            if (rawTurns.length > 0) {
              // no memory id: memory_get opens each by where it is (session + turns), with the
              // rest of its exchange
              sections.push(
                "Verbatim excerpts (memory_get with the session + turns given opens each with the rest of its exchange):\n" +
                  rawTurns
                    .map(
                      (x, i) =>
                        `${i + 1}. ${x.turn.speaker ? `${x.turn.speaker}: ` : ""}${x.turn.text.slice(0, 300)}` +
                        (x.turn.turn !== null && x.turn.turn !== undefined
                          ? ` (session="${x.sessionId}", turns="${String(x.turn.turn)}")` : ` (session="${x.sessionId}")`),
                    )
                    .join("\n"),
              );
            }
            api.logger.info(
              `[${PLUGIN_ID}] tool=memory_recall → no grounded facts, fell back to rollups=${rollups.length} rawTurns=${rawTurns.length}`,
            );
            return {
              content: [
                {
                  type: "text",
                  text: `No directly attributable memory, but related context exists:\n\n${sections.join("\n\n")}`,
                },
              ],
              details: {
                count: 0,
                fallback: "rollup_raw",
                rollups: rollups.map((r) => ({ topic: r.topic, summary: r.summary, score: r.score })),
                raw_turns: rawTurns.map((x) => ({
                  session_id: x.sessionId,
                  speaker: x.turn.speaker,
                  text: x.turn.text,
                  score: x.turn.score,
                })),
              },
            };
          }
          const lines = filtered
            .map((f, idx) => {
              const pct = ((f.score ?? 0) * 100).toFixed(0);
              const memoryId = pickAddressableMemoryId(f);
              // id 必须完整输出：memory_get 走精确匹配，截断即 404。
              return memoryId
                ? `${idx + 1}. [${f.support_level}] ${f.claim} (${pct}% id=${memoryId})`
                : `${idx + 1}. [${f.support_level}] ${f.claim} (${pct}%, no id — cannot be expanded)`;
            })
            .join("\n");
          // the newest turns, not extracted yet, are in raw_context beside the facts: show the best of them too
          const excerpts = pickUnextractedExcerpts(resp.raw_context, cfg.recallMinScore);
          const excerptText = formatUnextractedExcerpts(excerpts);
          if (excerpts.length > 0) {
            api.logger.info(`[${PLUGIN_ID}] tool=memory_recall → plus ${excerpts.length} unextracted excerpt(s)`);
          }
          return {
            content: [{
              type: "text",
              text: `Found ${filtered.length} memories:\n\n${lines}` + (excerptText ? `\n\n${excerptText}` : ""),
            }],
            details: {
              count: filtered.length,
              ...(excerpts.length > 0
                ? { raw_excerpts: excerpts.map((x) => ({ session_id: x.sessionId, turn: x.turn.turn, text: x.turn.text, score: x.turn.score })) }
                : {}),
              memories: filtered.map((f) => ({
                // id = 本次查询的临时包 id（不可寻址）；memory_id = 可喂给 memory_get 的 32 位 hex。
                id: f.id,
                memory_id: pickAddressableMemoryId(f),
                text: f.claim,
                support_level: f.support_level,
                score: f.score,
                source_memory_ids: f.source_memory_ids,
              })),
            },
          };
        } catch (err) {
          const detail = formatHeliconError(err);
          api.logger.warn(`[${PLUGIN_ID}] tool=memory_recall FAILED ${detail}`);
          return {
            content: [{ type: "text", text: `memory_recall failed: ${detail}` }],
            details: { error: detail },
          };
        }
      },
    }),
    { name: "memory_recall" },
  );

  // The agent's own writes: only when memoryStoreTool is on (config.ts — off by default: such a
  // write has no source turn, and the fact it states is stored again beside what capture extracts).
  if (cfg.memoryStoreTool) {
    api.registerTool(
      {
        name: "memory_store",
        label: "Memory Store (HeliconSearch)",
        description:
          "Save important information into the cross-session long-term semantic memory in HeliconSearch. Use for preferences, facts, decisions, named entities, or instructions worth remembering across sessions. Duplicates are detected on the server side.",
        parameters: Type.Object({
          text: Type.String({ description: "Information to remember (free text)" }),
          category: Type.Optional(
            Type.Unsafe<MemoryCategory>({
              type: "string",
              enum: [...MEMORY_CATEGORIES],
              description: "Memory category (default: other)",
            }),
          ),
          importance: Type.Optional(Type.Number({ description: "Importance 0-1 (default 0.7)" })),
        }),
        async execute(_toolCallId: string, raw: unknown) {
          const args = (raw ?? {}) as {
            text?: string;
            category?: MemoryCategory;
            importance?: number;
          };
          const text = (args.text ?? "").trim();
          const category = args.category ?? "other";
          const importance =
            typeof args.importance === "number" ? Math.max(0, Math.min(1, args.importance)) : 0.7;
          const started = Date.now();
          api.logger.info(
            `[${PLUGIN_ID}] tool=memory_store text="${text.slice(0, 80)}" category=${category} importance=${importance} agent=${cfg.agentId}`,
          );

          if (!text) {
            return {
              content: [{ type: "text", text: "memory_store: text is required" }],
              details: { error: "missing_text" },
            };
          }

          try {
            const result = await client.store({
              text,
              agent_id: cfg.agentId,
              project_id: cfg.projectId,
              category,
              importance,
              scope: "project",
              extraction_method: "agent_tool",
              confidence: 0.9,
              source_refs: [makeManualRef("agent_tool")],
            });
            const elapsedMs = Date.now() - started;
            api.logger.info(
              `[${PLUGIN_ID}] tool=memory_store → status=${result.status} id=${result.id} elapsedMs=${elapsedMs}`,
            );
            const msg =
              result.status === "dedupe"
                ? `Similar memory already exists (id=${result.id}).`
                : result.status === "created"
                  ? `Stored: "${text.slice(0, 100)}" (id=${result.id})`
                  : `Store unexpected status: ${result.status}`;
            return {
              content: [{ type: "text", text: msg }],
              details: { action: result.status, id: result.id },
            };
          } catch (err) {
            const detail = formatHeliconError(err);
            api.logger.warn(`[${PLUGIN_ID}] tool=memory_store FAILED ${detail}`);
            return {
              content: [{ type: "text", text: `memory_store failed: ${detail}` }],
              details: { error: detail },
            };
          }
        },
      },
      { name: "memory_store" },
    );
  } else {
    api.logger.info(`[${PLUGIN_ID}] memory_store tool skipped (memoryStoreTool=false)`);
  }

  api.registerTool(
    {
      name: "memory_forget",
      label: "Memory Forget (HeliconSearch)",
      description:
        "Delete a memory from HeliconSearch by id. When the id is unknown, give `query` instead: that only lists the candidate memories (nothing is deleted); call again with the `memoryId` of the one to forget.",
      parameters: Type.Object({
        memoryId: Type.Optional(
          Type.String({
            description:
              "32-character hex memory id from memory_recall or the MEMORY_IDS block. Never a `pkg:…` id — those are per-query references and cannot be deleted.",
          }),
        ),
        query: Type.Optional(Type.String({ description: "Search query to locate memory" })),
      }),
      async execute(_toolCallId: string, raw: unknown) {
        const args = (raw ?? {}) as { memoryId?: string; query?: string };
        const rawId = (args.memoryId ?? "").trim();
        const query = (args.query ?? "").trim();
        // 删除是破坏性操作：与 memory_get 用同一套 id 校验，宁可早报错也不要拿一个
        // 不可寻址的 id 去 DELETE，然后只回一句 ok=false。
        let memoryId = "";
        if (rawId) {
          const check = normalizeMemoryId(rawId);
          if (check.ok) {
            memoryId = check.id;
          } else if (!query) {
            api.logger.info(
              `[${PLUGIN_ID}] tool=memory_forget rejected unusable id="${rawId.slice(0, 24)}"`,
            );
            return {
              content: [{ type: "text", text: `memory_forget: ${check.hint}` }],
              details: { error: "invalid_memory_id", memoryId: rawId },
            };
          }
        }
        api.logger.info(
          `[${PLUGIN_ID}] tool=memory_forget memoryId=${memoryId || "?"} query="${query.slice(0, 60)}"`,
        );

        try {
          if (memoryId) {
            const ok = await client.delete(memoryId);
            api.logger.info(`[${PLUGIN_ID}] tool=memory_forget → deleted id=${memoryId} ok=${ok}`);
            return {
              content: [{ type: "text", text: ok ? `Memory ${memoryId} forgotten.` : `Memory ${memoryId} could not be forgotten (the server did not delete it).` }],
              details: { action: ok ? "deleted" : "not_deleted", id: memoryId, ok },
            };
          }
          if (!query) {
            return {
              content: [{ type: "text", text: "memory_forget: provide either memoryId or query" }],
              details: { error: "missing_param" },
            };
          }
          const resp = await client.query({
            query,
            agent_id: cfg.agentId,
            project_id: cfg.projectId,
            purpose: "active",
            top_k: 5,
            render: "none",
          }, { deadlineMs: recallBudgets.activeDeadlineMs() });
          recallBudgets.fromResponse(resp.execution);
          // memory_forget 只删有底层 memory 的带：grounded_facts + needs_verification
          // （带 source_memory_ids）。rollup / raw 无对应可删记忆，不参与。
          // memoryId 一律取 pickAddressableMemoryId：条目自带的 id 是本次查询的临时包 id，
          // 拿它去 DELETE 只会静默失败。
          const deletable = [
            ...(resp.grounded_facts ?? []).map((f) => ({
              memoryId: pickAddressableMemoryId(f), text: f.claim, score: f.score ?? 0,
              support_level: f.support_level,
            })),
            ...(resp.needs_verification ?? []).map((v) => ({
              memoryId: pickAddressableMemoryId(v), text: v.claim, score: v.score ?? 0,
              support_level: "",
            })),
          ];
          const filtered = deletable.filter(
            (it): it is typeof it & { memoryId: string } => it.score >= 0.05 && Boolean(it.memoryId),
          );
          if (filtered.length === 0) {
            api.logger.info(`[${PLUGIN_ID}] tool=memory_forget → no candidates for query`);
            return {
              content: [{ type: "text", text: "No matching memories found." }],
              details: { action: "no_candidates" },
            };
          }
          // A search never deletes: a score says how well a memory matched the words, not that it is the one
          // meant, and a deletion cannot be undone. The agent picks one and calls again with its id.
          const list = filtered
            .map(
              (it) => `- [${it.memoryId}] ${it.text.slice(0, 60)} (${(it.score * 100).toFixed(0)}%)`,
            )
            .join("\n");
          api.logger.info(
            `[${PLUGIN_ID}] tool=memory_forget → returned ${filtered.length} candidates`,
          );
          return {
            content: [
              {
                type: "text",
                text: `Found ${filtered.length} candidate${filtered.length === 1 ? "" : "s"}. Nothing was deleted: call memory_forget again with the memoryId of the one to forget:\n${list}`,
              },
            ],
            details: {
              action: "candidates",
              candidates: filtered.map((it) => ({
                id: it.memoryId,
                text: it.text,
                support_level: it.support_level,
                score: it.score,
              })),
            },
          };
        } catch (err) {
          const detail = formatHeliconError(err);
          api.logger.warn(`[${PLUGIN_ID}] tool=memory_forget FAILED ${detail}`);
          return {
            content: [{ type: "text", text: `memory_forget failed: ${detail}` }],
            details: { error: detail },
          };
        }
      },
    },
    { name: "memory_forget" },
  );

  // ── memory_get — a memory, and the conversation it was said in ──────
  // One tool for both layers: the stored record (text, version history, sources) and the archived
  // messages around where it was said — the assistant's replies too, which recall shows none of —
  // or the whole conversation page by page. Also opens a recalled verbatim excerpt (RAW_REFS:
  // session + turns), which has no memory id.
  api.registerTool(
    (toolCtx) => ({
      name: "memory_get",
      label: "Memory Get (HeliconSearch)",
      description:
        "Open one memory — by its 32-hex id (MEMORY_IDS block or memory_recall), or by query when it has no id — with its full text, older versions, " +
        "and the source message with a few messages around it (what the user said and what the assistant replied). Use it when you need that and the " +
        "shown memory does not give it; open only the memory you need. session + turns opens a verbatim excerpt (RAW_REFS) the same way. " +
        "Never pass a `pkg:…` id or a `<skill_id>` (use skill_get for those).",
      parameters: Type.Object({
        memoryId: Type.Optional(Type.String({
          description: "32-character hex memory id from memory_recall or the MEMORY_IDS block. Never a `pkg:…` id — those are per-query references and cannot be looked up.",
        })),
        query: Type.Optional(Type.String({
          description: "Natural-language description of the memory, used when its id is unknown. The best-matching memory is opened.",
        })),
        session: Type.Optional(Type.String({
          description: "A session id: with turns, the excerpt to open (from RAW_REFS); with memoryId, which of the memory's source sessions to show.",
        })),
        turns: Type.Optional(Type.String({
          description: "With session (no memoryId): the message(s) to open, as RAW_REFS lists them — \"12\" or \"12-14\".",
        })),
        window: Type.Optional(Type.Number({
          description: `Messages on each side of the source (default ${DEFAULT_CONTEXT_WINDOW}, at most ${MAX_CONTEXT_WINDOW}). Widen only when the messages shown stop short of what you need.`,
        })),
        context: optionalStringEnum(["around", "session"] as const, {
          description: `"around" (default): the messages around the source. "session": the whole conversation, ${SESSION_PAGE_SIZE} messages a page (fromTurn continues) — only when the user asks to go through that conversation.`,
        }),
        fromTurn: Type.Optional(Type.Number({
          description: "With context=\"session\": the message to start the page at (as the previous page says).",
        })),
      }),
      async execute(_toolCallId: string, raw: unknown) {
        const args = (raw ?? {}) as {
          memoryId?: string; query?: string; session?: string; turns?: string | number;
          window?: number; context?: string; fromTurn?: number;
        };
        const rawId = (args.memoryId ?? "").trim();
        const query = (args.query ?? "").trim();
        const wantSession = (args.session ?? "").trim();
        const mode: "around" | "session" = args.context === "session" ? "session" : "around";
        const window = typeof args.window === "number" && Number.isFinite(args.window)
          ? Math.max(0, Math.min(MAX_CONTEXT_WINDOW, Math.floor(args.window))) : DEFAULT_CONTEXT_WINDOW;
        const fromTurn = typeof args.fromTurn === "number" && Number.isFinite(args.fromTurn)
          ? Math.max(0, Math.floor(args.fromTurn)) : undefined;
        // this user message's budget (the same key before_prompt_build clears)
        const turnKey = toolCtx.sessionKey ?? toolCtx.sessionId ?? "";
        const openLimit = () => {
          const made = used(turnKey, "opens");
          api.logger.info(`[${PLUGIN_ID}] tool=memory_get → not opened: ${made} opened already for this message (session=${turnKey})`);
          return {
            content: [{
              type: "text" as const,
              text:
                `memory_get: not opened — ${made} memories were opened already for this message. Answer with ` +
                "what you have; if it does not settle the question, say what is missing.",
            }],
            details: { error: "open_limit", opens: made },
          };
        };

        // the conversation part, once the session and the source turns are known
        const conversation = async (sessionId: string, src: number[], marked: "memory source" | "excerpt" = "memory source"): Promise<{ text: string; details: Record<string, unknown> }> => {
          try {
            let page;
            if (mode === "session") {
              page = await client.getSessionTurns(sessionId, {
                agentId: cfg.agentId, projectId: cfg.projectId, fromTurn: fromTurn ?? 0, limit: SESSION_PAGE_SIZE,
              });
            } else if (src.length > 0) {
              // at most a page: sources spread far apart (a short quote matched in several
              // messages) are shown from the first, and the page says the conversation goes on
              const { from, to } = windowAround(src[0]!, src[src.length - 1]!, window);
              page = await client.getSessionTurns(sessionId, {
                agentId: cfg.agentId, projectId: cfg.projectId, fromTurn: from, toTurn: to,
                limit: Math.min(to - from + 1, SESSION_PAGE_SIZE),
              });
            } else {
              // a source session with no turn known: its opening page
              page = await client.getSessionTurns(sessionId, {
                agentId: cfg.agentId, projectId: cfg.projectId, fromTurn: 0, limit: SESSION_PAGE_SIZE,
              });
            }
            if (page.total > 0) {
              return {
                text: formatPage({ sessionId, page, sourceTurns: src, mode: src.length > 0 ? mode : "session", window, marked }),
                details: { session: sessionId, total: page.total, shown: page.turns.map((t) => t.turn_index), source_turns: src },
              };
            }
          } catch (err) {
            if (!(err instanceof HeliconApiError && err.status === 404)) {
              const detail = formatHeliconError(err);
              api.logger.warn(`[${PLUGIN_ID}] tool=memory_get conversation FAILED session=${sessionId} ${detail}`);
              return { text: `(the conversation of session ${sessionId} could not be read: ${detail})`, details: { session: sessionId, error: detail } };
            }
          }
          return { text: `(no archived conversation found for session ${sessionId})`, details: { session: sessionId, error: "not_archived" } };
        };

        // ── an excerpt by where it is: session + turns ──
        if (!rawId && !query) {
          if (!wantSession) {
            return {
              content: [{ type: "text", text: "memory_get: provide memoryId (32 hex chars), query, or session + turns." }],
              details: { error: "missing_param" },
            };
          }
          const span = args.turns === undefined ? null : parseTurns(args.turns);
          if (args.turns !== undefined && !span) {
            return {
              content: [{ type: "text", text: `memory_get: turns must be a message index ("12") or range ("12-14"), as RAW_REFS lists them.` }],
              details: { error: "invalid_turns", turns: args.turns },
            };
          }
          if (!take(turnKey, "opens")) {
            return openLimit();
          }
          api.logger.info(`[${PLUGIN_ID}] tool=memory_get session=${wantSession} turns=${String(args.turns ?? "-")} context=${mode} window=${window}`);
          // (hs returns at most 200 messages a call: a wider range is read as its first 200)
          const asked = span ? Array.from({ length: Math.min(span[1] - span[0] + 1, 200) }, (_, k) => span[0] + k) : [];
          const conv = await conversation(wantSession, asked, "excerpt");
          return { content: [{ type: "text", text: conv.text }], details: conv.details };
        }

        // ── a memory: by id, else by query ──
        let memoryId = "";
        let resolvedVia: "id" | "query" = "id";
        if (rawId) {
          const check = normalizeMemoryId(rawId);
          if (check.ok) {
            memoryId = check.id;
          } else if (!query) {
            api.logger.info(`[${PLUGIN_ID}] tool=memory_get rejected unusable id="${rawId.slice(0, 24)}"`);
            return { content: [{ type: "text", text: `memory_get: ${check.hint}` }], details: { error: "invalid_memory_id", memoryId: rawId } };
          } else {
            api.logger.info(`[${PLUGIN_ID}] tool=memory_get unusable id="${rawId.slice(0, 24)}" → resolving by query`);
          }
        }
        // finding the memory by query is a memory search: it counts with memory_recall's
        if (!memoryId && turnKey && used(turnKey, "searches") >= MAX_SEARCHES_PER_TURN) {
          const made = used(turnKey, "searches");
          api.logger.info(`[${PLUGIN_ID}] tool=memory_get → not searched: ${made} searches already for this message (session=${turnKey})`);
          return {
            content: [{
              type: "text",
              text:
                `memory_get: not searched — memory was already searched ${made} times for this message. Open a ` +
                "memory by an id you have (MEMORY_IDS, memory_recall) or answer with what you have.",
            }],
            details: { error: "recall_limit", calls: made },
          };
        }
        if (!take(turnKey, "opens")) {
          return openLimit();
        }
        if (!memoryId) {
          take(turnKey, "searches");
        }
        try {
          if (!memoryId) {
            // the recalled context does not always carry an id: open what is described
            api.logger.info(`[${PLUGIN_ID}] tool=memory_get resolving query="${query.slice(0, 60)}"`);
            const resp = await client.query(
              { query, agent_id: cfg.agentId, project_id: cfg.projectId, purpose: "active", top_k: 5, render: "none" },
              { deadlineMs: recallBudgets.activeDeadlineMs() });
            recallBudgets.fromResponse(resp.execution);
            const candidates = [
              ...(resp.grounded_facts ?? []).map((f) => ({ memoryId: pickAddressableMemoryId(f), score: f.score ?? 0 })),
              ...(resp.needs_verification ?? []).map((v) => ({ memoryId: pickAddressableMemoryId(v), score: v.score ?? 0 })),
            ]
              .filter((c): c is { memoryId: string; score: number } => Boolean(c.memoryId) && c.score >= cfg.recallMinScore)
              .sort((a, b) => b.score - a.score);
            if (candidates.length === 0) {
              api.logger.info(`[${PLUGIN_ID}] tool=memory_get → no addressable match for query`);
              return {
                content: [{
                  type: "text",
                  text: "memory_get: no stored memory matched that query. A recalled verbatim excerpt has no memory id — open it with session + turns from RAW_REFS.",
                }],
                details: { error: "no_match" },
              };
            }
            memoryId = candidates[0]!.memoryId;
            resolvedVia = "query";
          }
          const d = await client.explain(memoryId);
          if (!d?.memory) {
            return {
              content: [{ type: "text", text: `memory_get: no memory with id ${memoryId} (it may have been deleted or superseded). Call memory_recall for a current id.` }],
              details: { error: "not_found", memoryId },
            };
          }
          const mem = d.memory;
          const chain = d.supersedes_chain ?? [];
          const source = await client.getSource(memoryId).catch((err: unknown) => {
            api.logger.warn(`[${PLUGIN_ID}] tool=memory_get source FAILED id=${memoryId} ${formatHeliconError(err)}`);
            return null;
          });
          const turnsBySession = sourceTurns(source);
          const sessions = sourceSessions(source, turnsBySession);
          const lines: string[] = [
            `**Memory** id=${mem.id}`,
            `Text: ${mem.text}`,
            `Type: ${mem.memory_type} | Category: ${mem.category} | Importance: ${String(mem.importance)}`,
            `Source completeness: ${d.source_completeness}`,
            `Source sessions: ${sessions.length > 0
              ? sessions.map((s) => {
                const t = turnsBySession.get(s) ?? [];
                return t.length > 0 ? `${s} (messages ${t.map((i) => `#${i}`).join(", ")})` : s;
              }).join("; ")
              : "none"}`,
          ];
          if (chain.length > 0) {
            lines.push(`\nVersion history (${chain.length} older version${chain.length > 1 ? "s" : ""}):`);
            chain.forEach((c, i) => lines.push(`  ${i + 1}. id=${c.memory_id} (${c.lifecycle_state}) ${c.text}`));
          }
          let conv: { text: string; details: Record<string, unknown> } | null = null;
          const target = wantSession && sessions.includes(wantSession) ? wantSession : sessions[0];
          if (wantSession && target !== wantSession) {
            lines.push(`(session ${wantSession} is not a source of this memory${target ? ` — showing ${target}` : ""})`);
          }
          if (target) {
            conv = await conversation(target, turnsBySession.get(target) ?? []);
            lines.push("", conv.text);
            if (sessions.length > 1) {
              lines.push(`(also said in: ${sessions.filter((s) => s !== target).join(", ")} — pass session to show one of them)`);
            }
          } else {
            lines.push("", "(no source conversation is recorded for this memory)");
          }
          api.logger.info(
            `[${PLUGIN_ID}] tool=memory_get id=${memoryId} via=${resolvedVia} chain=${chain.length} sessions=${sessions.length} ` +
            `session=${target ?? "-"} context=${mode} window=${window}`,
          );
          return {
            content: [{ type: "text", text: lines.join("\n") }],
            details: { memory: mem, supersedes_chain: chain, source_refs: d.source_refs, sessions, conversation: conv?.details ?? null },
          };
        } catch (err) {
          const detail = formatHeliconError(err);
          api.logger.warn(`[${PLUGIN_ID}] tool=memory_get FAILED ${detail}`);
          return { content: [{ type: "text", text: `memory_get failed: ${detail}` }], details: { error: detail } };
        }
      },
    }),
    { name: "memory_get" },
  );

  // ── L2.5: skill_get — fetch published SKILL.md body ────────────────
  // Same host-stays-in-client pattern as memory_get. The recall prompt
  // injects <skill_id> only; this tool calls HeliconClient.getSkill(skillId)
  // which returns HS's `data.content` markdown body.
  api.registerTool(
    {
      name: "skill_get",
      label: "Skill Get (HeliconSearch)",
      description:
        "Retrieve the SKILL.md body of a published skill matched by HeliconSearch auto-recall. " +
        "Pass the 32-hex `skill_id` from the `<recalled_skills>` block in injected context " +
        "(e.g. `2e24aed6c17248949926b5bfe285781f`). These are remote records, not local files — " +
        "never read or guess a file path for them. " +
        "Returns the markdown guidance body — apply it to the current task. " +
        "Do not call web_fetch or curl: the host URL is private and must not be exposed to the model.",
      parameters: Type.Object({
        skillId: Type.String({
          description:
            "The 32-hex skill_id from the <recalled_skills> block in injected recall context " +
            "(e.g. `2e24aed6c17248949926b5bfe285781f`). Use the <skill_id> value verbatim, " +
            "NOT the <name> slug and NOT a filesystem path.",
        }),
      }),
      async execute(_toolCallId: string, raw: unknown) {
        const args = (raw ?? {}) as { skillId?: string };
        const skillId = (args.skillId ?? "").trim();
        // skillId 在 HS 端是 32-hex —— /v1/memory/skill/<id> 与 /v1/memory/<id>
        // 在服务端完全独立，透传即可。URL/路径字符被拒绝，防止 skillId 被误传
        // 为 <location> URL 让我们泄露 HS host。
        if (!skillId) {
          return {
            content: [{ type: "text", text: "skill_get: skillId is required." }],
            details: { error: "missing_param" },
          };
        }
        if (/[/?#\s]/.test(skillId) || /^[a-z]+:\/\//i.test(skillId)) {
          api.logger.info(
            `[${PLUGIN_ID}] tool=skill_get rejected suspicious skillId="${skillId.slice(0, 40)}"`,
          );
          return {
            content: [{ type: "text", text: "skill_get: skillId must be the 32-hex <skill_id> value from <recalled_skills>, not a URL or path." }],
            details: { error: "invalid_skill_id", skillId },
          };
        }
        api.logger.info(`[${PLUGIN_ID}] tool=skill_get skillId=${skillId}`);
        try {
          const result = await client.getSkill(skillId);
          if (!result) {
            return {
              content: [
                {
                  type: "text",
                  text: `skill_get: no published skill with id ${skillId} (it may have been retracted or renamed).`,
                },
              ],
              details: { error: "not_found", skillId },
            };
          }
          const body = result.content ?? "";
          if (!body) {
            return {
              content: [{ type: "text", text: `skill_get: skill ${skillId} returned an empty body.` }],
              details: { error: "empty_body", skillId },
            };
          }
          // 返回正文 markdown。包一层简短 header 便于模型在多 skill 场景里区分。
          const text = `**Skill** id=${skillId}\n\n${body}`;
          api.logger.info(
            `[${PLUGIN_ID}] tool=skill_get skillId=${skillId} bytes=${body.length}`,
          );
          return {
            content: [{ type: "text", text }],
            details: { skillId, bytes: body.length },
          };
        } catch (err) {
          const detail = formatHeliconError(err);
          api.logger.warn(`[${PLUGIN_ID}] tool=skill_get FAILED ${detail}`);
          return {
            content: [{ type: "text", text: `skill_get failed: ${detail}` }],
            details: { error: detail },
          };
        }
      },
    },
    { name: "skill_get" },
  );
}

// ── CLI ──────────────────────────────────────────────────────────────────────
