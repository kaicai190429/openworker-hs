/**
 * The time a turn's memory work may take (MEMORY_INFERENCE_SCHEDULING_ADAPTIVE_RECALL_PLAN.md §8):
 *
 * - HookBudget: made at the start of before_prompt_build; bounds that turn's SessionBridge check /
 *   fetch, the automatic recall (request and body read) and the composing. Its deadline is the
 *   automatic tier's budget plus a transport margin, and always inside the host's hook timeout (the
 *   host only races the hook — it cancels nothing — so work past it is wasted). The recall is sent
 *   what is left as budget_ms: the server only ever shortens its budget to it.
 * - RecallBudgets: the server's configured budgets (automatic tier, active retrieval), per backend,
 *   from /v1/memory/health at start-up and from the responses' "execution" — updated only when the
 *   configuration fingerprint changes; a response's effective budget is never written back. Until
 *   they are known, or when the server runs none (an older server, a benchmark instance: no
 *   "execution"), fixed start-up values from the host's hook timeout.
 */

import type { ClientLogger, HeliconHealth, RecallExecution } from "./helicon-client.js";

const PLUGIN_ID = "helicon-memory";

/** network, JSON and composing: what the plugin needs beyond the server's own budget */
export const TRANSPORT_MARGIN_MS = 800;
/** the hook ends this much before the host's timeout */
export const HOST_SAFETY_MS = 300;
/** the SessionBridge is waited for until this much before the turn's deadline (then omitted) */
export const COMPOSE_MARGIN_MS = 100;
/** less than this left: the recall is not sent (it could not end in time) */
export const MIN_RECALL_BUDGET_MS = 200;
/** the host's before_prompt_build timeout when nothing configures another (openclaw hooks.ts) */
export const DEFAULT_HOOK_TIMEOUT_MS = 15_000;
/** the active retrieval's start-up value is at most this (less the transport margin) */
const ACTIVE_START_CAP_MS = 60_000;

function positive(v: unknown): number | undefined {
  return typeof v === "number" && Number.isFinite(v) && v > 0 ? Math.floor(v) : undefined;
}

/**
 * The host's timeout for this plugin's hook, as the host resolves it when the hook is registered
 * (openclaw registry.ts resolveTypedHookTimeoutMs): plugins.entries.<id>.hooks.timeouts.<hook>,
 * then .hooks.timeoutMs; this plugin passes no api.on timeoutMs; else the default.
 */
export function hostHookTimeoutMs(openclawConfig: unknown, pluginId: string, hookName: string): number {
  const hooks = (openclawConfig as {
    plugins?: { entries?: Record<string, { hooks?: { timeouts?: Record<string, unknown>; timeoutMs?: unknown } }> };
  } | undefined)?.plugins?.entries?.[pluginId]?.hooks;
  return positive(hooks?.timeouts?.[hookName]) ?? positive(hooks?.timeoutMs) ?? DEFAULT_HOOK_TIMEOUT_MS;
}

export type BudgetSource = "health" | "response" | "startup";

export type Budgets = {
  /** the automatic tier's budget, ms */
  autoMs: number;
  /** the active retrieval's budget, ms */
  activeMs: number;
  source: BudgetSource;
  fingerprint: string | null;
};

/** The start-up values: from the host's hook timeout alone. */
export function startupBudgets(hostTimeoutMs: number): Budgets {
  const autoMs = Math.max(0, hostTimeoutMs - TRANSPORT_MARGIN_MS - HOST_SAFETY_MS);
  return {
    autoMs,
    activeMs: Math.min(3 * autoMs, ACTIVE_START_CAP_MS - TRANSPORT_MARGIN_MS),
    source: "startup",
    fingerprint: null,
  };
}

type BudgetState = {
  cached: Budgets | null;
  /** why the start-up values are in use (said once per change) */
  startupWhy: string | null;
};

/**
 * The budgets per backend address, for the process: the host registers a plugin more than once
 * (one registry for the gateway's services, another for its agent runs), so what the service's
 * start-up health probe read must reach the hooks of every registration.
 */
const BY_BACKEND = new Map<string, BudgetState>();

/** The server's configured budgets for one backend address, else the start-up values. */
export class RecallBudgets {
  private readonly state: BudgetState;

  constructor(
    readonly baseUrl: string,
    readonly hostTimeoutMs: number,
    private readonly logger: ClientLogger,
  ) {
    let st = BY_BACKEND.get(baseUrl);
    if (!st) {
      st = { cached: null, startupWhy: null };
      BY_BACKEND.set(baseUrl, st);
    }
    this.state = st;
  }

  private get cached(): Budgets | null {
    return this.state.cached;
  }

  private set cached(v: Budgets | null) {
    this.state.cached = v;
  }

  private get startupWhy(): string | null {
    return this.state.startupWhy;
  }

  private set startupWhy(v: string | null) {
    this.state.startupWhy = v;
  }

  current(): Budgets {
    return this.cached ?? startupBudgets(this.hostTimeoutMs);
  }

  /** active retrieval: the deadline of a search started now (the server's budget + transport) */
  activeDeadlineMs(now: number = Date.now()): number {
    return now + this.current().activeMs + TRANSPORT_MARGIN_MS;
  }

  /** /v1/memory/health's recall section (null: the health call failed) */
  fromHealth(h: HeliconHealth | null): void {
    const r = h?.recall;
    if (!r) {
      this.useStartup(h === null ? "health not answered" : "no recall budgets in health (older server)");
      return;
    }
    this.take(r.auto_tier_budget_ms, r.active_budget_ms, r.config_fingerprint, "health",
              r.error ? `health recall error: ${r.error}` : "health without budgets");
  }

  /** a recall response's "execution" (undefined: the server ran no budgets) */
  fromResponse(exec: RecallExecution | undefined): void {
    if (!exec) {
      this.useStartup("a response without execution (older server or benchmark instance)");
      return;
    }
    // (effective_budget_ms is this recall's own, shortened by what was sent: never written back)
    this.take(exec.auto_tier_budget_ms, exec.active_budget_ms, exec.config_fingerprint, "response",
              "execution without budgets");
  }

  private take(auto: unknown, active: unknown, fp: unknown, source: BudgetSource, invalidWhy: string): void {
    const autoMs = positive(auto), activeMs = positive(active);
    const fingerprint = typeof fp === "string" && fp ? fp : null;
    if (autoMs === undefined || activeMs === undefined || fingerprint === null) {
      this.useStartup(invalidWhy);
      return;
    }
    if (this.cached && this.cached.fingerprint === fingerprint) {
      return;                     // the same configuration: nothing to update
    }
    const before = this.cached;
    this.cached = { autoMs, activeMs, source, fingerprint };
    this.startupWhy = null;
    this.logger.info(
      `[${PLUGIN_ID}] recall budgets for ${this.baseUrl}: automatic ${autoMs}ms, active ${activeMs}ms ` +
      `(fingerprint ${fingerprint}, from ${source}${before ? `; was ${before.autoMs}/${before.activeMs}ms` : ""})`,
    );
  }

  private useStartup(why: string): void {
    const had = this.cached !== null;
    this.cached = null;
    if (had || this.startupWhy !== why) {
      const s = startupBudgets(this.hostTimeoutMs);
      this.logger.warn(
        `[${PLUGIN_ID}] recall budgets for ${this.baseUrl}: start-up values (automatic ${s.autoMs}ms, ` +
        `active ${s.activeMs}ms) — ${why}${had ? "; the cached budgets were dropped" : ""}`,
      );
    }
    this.startupWhy = why;
  }
}

/** One before_prompt_build's time (made at its start; a configuration read later does not reset it). */
export class HookBudget {
  readonly startedAt: number;
  readonly deadlineMs: number;

  constructor(
    readonly hostTimeoutMs: number,
    readonly budgets: Budgets,
    now: number = Date.now(),
  ) {
    this.startedAt = now;
    this.deadlineMs = now + Math.max(
      0, Math.min(budgets.autoMs + TRANSPORT_MARGIN_MS, hostTimeoutMs - HOST_SAFETY_MS));
  }

  /** what the recall sent now may take on the server: the deadline less the transport margin */
  recallBudgetMs(now: number = Date.now()): number {
    return Math.floor(this.deadlineMs - now - TRANSPORT_MARGIN_MS);
  }

  /** the SessionBridge is waited for until this time, then omitted */
  composeByMs(): number {
    return this.deadlineMs - COMPOSE_MARGIN_MS;
  }

  elapsedMs(now: number = Date.now()): number {
    return now - this.startedAt;
  }
}
