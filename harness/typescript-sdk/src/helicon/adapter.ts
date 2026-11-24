/**
 * The Helicon memory adapter: long-term memory for harness agents, served by HeliconSearch.
 *
 * It lives inside the TypeScript SDK and is switched on by a configuration file (config-file.ts);
 * without one nothing here runs and the SDK behaves exactly as before. It does not change how the
 * SDK works or what an application calls — it listens at a few points the SDK already has:
 *
 *   Harness.start        read the configuration, register the memory tools, listen to the journal
 *   Session.send / run   before a message is sent: recall relevant memories into that turn's
 *                        `instructionAddendum` (the SDK's own per-turn field, not stored in history)
 *   journal turn_completed   archive the new history and extract facts (capture.ts)
 *   Session.close        flush, then end the session on HeliconSearch (it writes the summary)
 *   Harness.shutdown     flush what is still pending
 */
import { createHash } from "node:crypto";
import { mkdir, readdir, stat, unlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { ToolImageResult, type ApplicationTools } from "../types.js";
import { CaptureEngine, toHeliconSessionId } from "./capture.js";
import { createDirectStrategy, createOperatorStrategy, type CaptureStrategy } from "./capture-strategy.js";
import type { MemoryConfig } from "./config.js";
import { loadHeliconConfig } from "./config-file.js";
import { MODEL_TOOL_PREFIX, memoryGuidance, TOOL_NAMESPACE } from "./guidance.js";
import { createHeliconClient, type ClientLogger, type HeliconClient } from "./helicon-client.js";
import { wrapRelevantMemoriesBlock } from "./filters.js";
import { createFileLogger } from "./log.js";
import { HeliconMediaClient } from "./media-client.js";
import { registerMediaTools } from "./media-tools.js";
import { performAutoRecall } from "./recall.js";
import { HookBudget, RecallBudgets } from "./recall-budget.js";
import { BRIDGE_PROTOCOL, formatBridgeContext, freezeBridgeBy, type BridgeTurn } from "./session-bridge.js";
import { imageExtension, type ImageAttachment } from "./registry.js";
import { SessionStore } from "./session-store.js";
import { registerTools, formatHeliconError, type ToolCtx, type ToolDef, type ToolHost } from "./tools.js";

const PLUGIN_ID = "helicon-memory";
/** The engine accepts at most this much application addendum per turn (UTF-8 bytes). */
const MAX_ADDENDUM_BYTES = 32_000;
const SUBMISSION_CACHE_MAX = 256;
/** `close()` holds the application for the memory work no longer than the extraction wait plus this. */
const CLOSE_MARGIN_MS = 15_000;
/** `shutdown()` holds the application no longer than this for the memory work. */
const SHUTDOWN_BUDGET_MS = 45_000;
/** Sessions of earlier runs ended per start. */
const ORPHANS_PER_START = 10;
/** A turn in flight keeps its session from being called idle for this long at most (a turn whose end was never seen must not keep a session open for ever). */
const RUNNING_CAP_MS = 6 * 3_600_000;
/**
 * What a tool hands the model at most (characters): a page of a long conversation, or a skill, can be far larger
 * than is useful, and it would all be paid for in the context of every later step.
 */
const MAX_TOOL_OUTPUT_CHARS = 60_000;

function capText(out: unknown): unknown {
  if (typeof out !== "string" || out.length <= MAX_TOOL_OUTPUT_CHARS) return out;
  let cut = out.slice(0, MAX_TOOL_OUTPUT_CHARS);
  const last = cut.charCodeAt(cut.length - 1);
  if (last >= 0xd800 && last <= 0xdbff) cut = cut.slice(0, -1); // not in the middle of a surrogate pair
  return `${cut}\n(cut here: the answer was ${out.length} characters, more than the ${MAX_TOOL_OUTPUT_CHARS} a tool result is given. Ask for less: a narrower query, a smaller window, or the next page.)`;
}

/** The journal events that end a turn, however it ended. */
const TURN_ENDED = new Set(["turn_completed", "turn_failed", "turn_cancelled", "turn_recovered"]);
/** Attached images are deleted after this long. */
const UPLOAD_KEEP_MS = 7 * 24 * 3_600_000;

/** What the adapter needs of a Harness (the real class satisfies it). */
export interface HarnessLike {
  request<T = unknown>(method: string, params: unknown): Promise<T>;
  registerTools(registration: ApplicationTools): Promise<void>;
  onJournalHint(listener: (method: string, params: unknown) => void): () => void;
  readonly capabilities: Readonly<Record<string, unknown>>;
}

/** The agent fields the adapter adjusts. */
export interface DecoratableAgent {
  name?: string;
  instructions: string;
  tools: string[];
  memory?: boolean;
  memoryScope?: string;
}

function utf8Length(text: string): number {
  return Buffer.byteLength(text, "utf8");
}

/** Cuts text to at most `maxBytes` of UTF-8 without splitting a character. */
function cutBytes(text: string, maxBytes: number): string {
  if (maxBytes <= 0) return "";
  if (utf8Length(text) <= maxBytes) return text;
  let end = Math.min(text.length, maxBytes);
  while (end > 0 && utf8Length(text.slice(0, end)) > maxBytes) end--;
  const last = text.charCodeAt(end - 1);
  if (last >= 0xd800 && last <= 0xdbff) end--;
  return text.slice(0, end);
}

/** What ends a cut addendum: its last `<relevant-memories>` envelope is kept closed. */
const ENVELOPE_END = "\n</relevant-memories>";

/**
 * `text` (blocks that each end with `</relevant-memories>`) within `room` bytes. A cut keeps the last envelope
 * closed, so what follows the addendum (the engine's own notes) is not read as part of it.
 */
function fitEnvelopes(text: string, room: number): string | undefined {
  if (utf8Length(text) <= room) return text;
  const keep = room - utf8Length(ENVELOPE_END);
  if (keep <= 0) return undefined;
  const cut = cutBytes(text, keep);
  return cut.trim() ? `${cut}${ENVELOPE_END}` : undefined;
}

/** A Map that forgets its oldest entry past a size, for per-turn bookkeeping. */
class BoundedMap<V> extends Map<string, V> {
  constructor(private readonly max: number) {
    super();
  }
  override set(key: string, value: V): this {
    if (!this.has(key) && this.size >= this.max) {
      const oldest = this.keys().next().value;
      if (oldest !== undefined) this.delete(oldest);
    }
    return super.set(key, value);
  }
}

export class HeliconAdapter {
  readonly cfg: MemoryConfig;
  /** Qualified names an agent lists in `tools` to get the memory tools. */
  readonly toolNames: string[];

  private readonly client: HeliconClient;
  private readonly logger: ClientLogger;
  private readonly store: SessionStore;
  private readonly capture: CaptureEngine;
  private readonly budgets: RecallBudgets;
  private readonly guidance: string;
  private readonly media: HeliconMediaClient | undefined;
  private readonly uploadsDir: string;
  /** files the user attached, by session: media_upload with no paths adds these */
  private readonly attached = new BoundedMap<string[]>(200);
  private readonly recalled = new Map<string, string | undefined>();
  private unsubscribe: (() => void) | undefined;
  private idleTimer: ReturnType<typeof setInterval> | undefined;
  /** sessions this process created, resumed or took a turn in, and has not closed: what shutdown and idling look at */
  private readonly known = new Set<string>();

  private constructor(
    private readonly harness: HarnessLike,
    cfg: MemoryConfig,
    stateDir: string,
  ) {
    this.cfg = cfg;
    const root = join(stateDir, "helicon-memory");
    this.logger = createFileLogger(join(root, "adapter.log"), cfg.debug);
    this.client = createHeliconClient({
      baseUrl: cfg.baseUrl,
      apiKey: cfg.apiKey,
      timeoutMs: cfg.timeoutMs,
      logger: this.logger,
      debug: cfg.debug,
    });
    this.store = new SessionStore(root, (message) => this.logger.warn(`[${PLUGIN_ID}] ${message}`));
    this.budgets = new RecallBudgets(cfg.baseUrl, cfg.hookTimeoutMs, this.logger);
    const strategy: CaptureStrategy =
      cfg.captureMode === "operator" ? createOperatorStrategy(this.client) : createDirectStrategy(this.client);
    this.capture = new CaptureEngine({
      cfg,
      client: this.client,
      strategy,
      store: this.store,
      request: (method, params) => harness.request(method, params),
      logger: this.logger,
      ownToolPrefix: MODEL_TOOL_PREFIX,
    });
    this.uploadsDir = join(root, "uploads");
    this.media = cfg.autoMedia ? new HeliconMediaClient({ baseUrl: cfg.baseUrl, apiKey: cfg.apiKey, timeoutMs: 30_000 }) : undefined;
    const names = [
      "memory_recall", "memory_get", "memory_forget", "skill_get",
      ...(cfg.memoryStoreTool ? ["memory_store"] : []),
      ...(this.media
        ? ["media_search", "media_upload", ...(cfg.mediaAdminTools ? ["media_search_by_image", "media_libraries", "media_files", "media_statistics"] : [])]
        : []),
    ];
    this.toolNames = names.map((n) => `${TOOL_NAMESPACE}.${n}`);
    this.guidance = memoryGuidance(cfg, new Set(names));
  }

  /**
   * Attaches the adapter to a started harness when a configuration file asks for it; returns
   * undefined (and changes nothing) otherwise. Never throws: memory must not stop an application.
   */
  static async attach(harness: HarnessLike, stateDir: string): Promise<HeliconAdapter | undefined> {
    let loaded;
    try {
      loaded = loadHeliconConfig(stateDir);
    } catch (err) {
      process.emitWarning(`helicon-memory: configuration could not be read: ${String(err)}`);
      return undefined;
    }
    if (loaded.status === "absent" || loaded.status === "disabled") return undefined;
    if (loaded.status === "invalid") {
      process.emitWarning(`helicon-memory is off: ${loaded.path}: ${loaded.reason}`);
      return undefined;
    }
    const caps = harness.capabilities;
    if (caps.application_callbacks !== true || caps.turn_context !== true) {
      process.emitWarning("helicon-memory is off: this harness build lacks application callbacks or turn context");
      return undefined;
    }
    try {
      const adapter = new HeliconAdapter(harness, loaded.cfg, stateDir);
      await adapter.start();
      return adapter;
    } catch (err) {
      process.emitWarning(`helicon-memory is off: ${err instanceof Error ? err.message : String(err)}`);
      return undefined;
    }
  }

  private async start(): Promise<void> {
    await this.harness.registerTools(this.buildTools());
    this.unsubscribe = this.harness.onJournalHint((method, params) => this.onJournal(method, params));
    // The server's recall budgets, and a first sign of whether it is there; the host does not wait for it.
    void this.client
      .health()
      .then((h) => this.budgets.fromHealth(h))
      .catch((err) => {
        this.budgets.fromHealth(null);
        this.logger.warn(`[${PLUGIN_ID}] HeliconSearch not reachable at ${this.cfg.baseUrl}: ${formatHeliconError(err)}`);
      });
    void this.pruneUploads();
    if (this.cfg.idleEndMs > 0 && this.cfg.syncTranscript) {
      this.idleTimer = setInterval(() => this.sweepIdle(), Math.min(60_000, Math.max(1_000, Math.floor(this.cfg.idleEndMs / 2))));
      this.idleTimer.unref?.();
      void this.endOrphans();
    }
    this.logger.info(`[${PLUGIN_ID}] adapter on: ${this.cfg.baseUrl} agent=${this.cfg.agentId} mode=${this.cfg.captureMode}`);
  }

  // ── tools ────────────────────────────────────────────────────────────────

  private buildTools(): ApplicationTools {
    const factories = new Map<string, (ctx: ToolCtx) => ToolDef>();
    const host: ToolHost = {
      logger: this.logger,
      registerTool: (tool, opts) => {
        const factory = typeof tool === "function" ? tool : () => tool;
        factories.set(opts?.name ?? factory({}).name, factory);
      },
    };
    // one counter per turn: the key carries the turn id, so a new user message starts afresh
    const turnUse = new BoundedMap<{ searches: number; opens: number }>(512);
    registerTools({ api: host, cfg: this.cfg, client: this.client, sessionState: { turnUse }, recallBudgets: this.budgets });
    if (this.media) {
      registerMediaTools({ api: host, client: this.media, cfg: this.cfg, attachedFiles: (id) => this.attached.get(id) ?? [] });
    }

    const tools: ApplicationTools["tools"] = {};
    for (const [name, factory] of factories) {
      const probe = factory({});
      tools[name] = {
        description: probe.description,
        inputSchema: probe.parameters as never,
        execute: async (args, ctx) => {
          const tool = factory({ sessionKey: `${ctx.sessionId}#${ctx.turnId}`, sessionId: ctx.sessionId });
          const result = await tool.execute(ctx.callId, args);
          const out = capText(result.data ?? result.content.map((c) => c.text).join("\n")) as never;
          return result.image ? new ToolImageResult(out, { mimeType: result.image.mimeType, base64: result.image.base64 }) : out;
        },
      };
    }
    return { namespace: TOOL_NAMESPACE, tools };
  }

  /** The agent definition with the memory tools and their usage rules added. */
  decorateAgent<T extends DecoratableAgent>(definition: T): T {
    if (!this.allows(definition.name)) return definition;
    const tools = [...new Set([...(definition.tools ?? []), ...this.toolNames])];
    const instructions = this.guidance ? `${definition.instructions}\n\n${this.guidance}` : definition.instructions;
    // the engine's own memory (and its scope, which only makes sense with it) gives way to this one
    const builtin = this.cfg.replaceBuiltinMemory ? { memory: false, memoryScope: undefined } : {};
    return { ...definition, tools, instructions, ...builtin };
  }

  /** Whether an agent of this name gets memory (`agents` in the configuration; absent: all). */
  private allows(agent: string | undefined): boolean {
    return !this.cfg.agents || (agent !== undefined && this.cfg.agents.includes(agent));
  }

  /** Sessions of agents that do not get memory: nothing of them is recalled, archived or ended. */
  private readonly excluded = new Set<string>();

  /**
   * Sessions that went through the SDK's own `Session` (the lead of a team). The SDK attaches a team's worker
   * sessions as well, so their journal events reach the adapter: they are not ours, and are neither recalled
   * into nor captured.
   */
  private readonly leads = new Set<string>();

  /** When each session's activity was last written to its file (activity itself is noted in memory at once). */
  private readonly savedAt = new Map<string, number>();

  /** Sessions with a turn in flight (when it was sent): a turn that waits for an approval, a slow tool or a slow model is quiet, but the session is not idle. */
  private readonly running = new Map<string, number>();

  // ── recall ───────────────────────────────────────────────────────────────

  private async fetchBridge(budget: HookBudget): Promise<BridgeTurn> {
    const cfg = this.cfg;
    try {
      const bridge = await this.client.getSessionBridge(
        {
          agentId: cfg.agentId,
          projectId: cfg.projectId,
          episodicLimit: cfg.sessionBridgeEpisodicLimit,
          pinnedMinImportance: cfg.sessionBridgePinnedMinImportance,
          pinnedMinRecall: cfg.sessionBridgePinnedMinRecall,
          pinnedTopK: cfg.sessionBridgePinnedTopK,
          procedureLimit: cfg.sessionBridgeProcedureLimit,
        },
        { deadlineMs: budget.deadlineMs },
      );
      if (bridge.protocol !== BRIDGE_PROTOCOL) {
        return { state: "omitted", why: `protocol ${bridge.protocol}` };
      }
      const rendering = formatBridgeContext(bridge, cfg.sessionBridgeTokenBudget);
      return rendering.text ? { state: "validated_fresh", rendering } : { state: "omitted", why: "empty" };
    } catch (err) {
      return { state: "omitted", why: formatHeliconError(err) };
    }
  }

  /**
   * What to add to this turn's `instructionAddendum`: the memories recalled for the user's message,
   * and — on a new session's first turn only — the cross-session bridge. The caller's own addendum
   * is kept first. A retry of the same submission gets the same text. Never throws.
   * (With `recallPlacement: "user"` the memories are not in the addendum: see `prepareTurnParts`.)
   */
  async prepareTurn(args: {
    sessionId: string;
    text: string;
    submissionId: string;
    userAddendum?: string | undefined;
    /** images the user attached to this message */
    images?: ImageAttachment[];
  }): Promise<string | undefined> {
    return (await this.prepareTurnParts(args)).addendum;
  }

  /**
   * The turn's addendum and the text for the front of the user's message. `recallPlacement: "user"` (the default): the
   * addendum stays the caller's own and the memories go in front of the user's message, followed by a blank line.
   * `"system"`: the recalled memories join the addendum, the caller's own first. Either way a retry of the same
   * submission gets the same text. Never throws.
   */
  async prepareTurnParts(args: {
    sessionId: string;
    text: string;
    submissionId: string;
    userAddendum?: string | undefined;
    images?: ImageAttachment[];
  }): Promise<{ addendum: string | undefined; userPrefix: string | undefined }> {
    const { sessionId, text, submissionId, userAddendum } = args;
    const plain = { addendum: userAddendum, userPrefix: undefined };
    if (this.excluded.has(sessionId)) return plain;
    try {
      this.touch(sessionId);
      this.running.set(sessionId, Date.now());
      if (args.images?.length) await this.keepAttachments(sessionId, args.images);
      const key = `${sessionId}:${submissionId}`;
      let ours: string | undefined;
      if (this.recalled.has(key)) {
        ours = this.recalled.get(key);
      } else {
        ours = await this.recallFor(sessionId, text);
        if (this.recalled.size >= SUBMISSION_CACHE_MAX) {
          const oldest = this.recalled.keys().next().value;
          if (oldest !== undefined) this.recalled.delete(oldest);
        }
        this.recalled.set(key, ours);
      }
      // the engine refuses a NUL in the addendum
      ours = ours?.replaceAll("\0", "");
      if (!ours?.trim()) return plain;
      if (this.cfg.recallPlacement === "user") {
        // in front of the user's message; the turn's addendum stays what the caller gave
        const fitted = fitEnvelopes(ours, MAX_ADDENDUM_BYTES);
        return { addendum: userAddendum, userPrefix: fitted ? `${fitted}\n\n` : undefined };
      }
      const own = userAddendum ?? "";
      const room = MAX_ADDENDUM_BYTES - utf8Length(own) - (own ? 2 : 0);
      const fitted = fitEnvelopes(ours, room);
      if (!fitted) return plain;
      return { addendum: own ? `${own}\n\n${fitted}` : fitted, userPrefix: undefined };
    } catch (err) {
      this.logger.warn(`[${PLUGIN_ID}] recall skipped: ${formatHeliconError(err)}`);
      return plain;
    }
  }

  private async recallFor(sessionId: string, text: string): Promise<string | undefined> {
    const cfg = this.cfg;
    const rec = this.store.get(sessionId);
    const wantBridge = cfg.sessionBridge && rec.bridgePending;
    if (!cfg.autoRecall && !wantBridge) return undefined;

    const budget = new HookBudget(cfg.hookTimeoutMs, this.budgets.current());
    const bridgeTurn: Promise<BridgeTurn> = wantBridge
      ? this.fetchBridge(budget)
      : Promise.resolve<BridgeTurn>({ state: "omitted", why: "not the first turn" });
    const frozen = freezeBridgeBy(bridgeTurn, budget.composeByMs());

    const recall = cfg.autoRecall
      ? await performAutoRecall({
          client: this.client,
          cfg,
          prompt: text,
          messages: [],
          logger: this.logger,
          hookBudget: budget,
          budgets: this.budgets,
          sessionKey: sessionId,
          bridgeShown: () => frozen.then((b) => (b.state === "omitted" ? null : b.rendering.shown)),
        })
      : { report: { outcome: "skipped" as const }, prependContext: undefined };
    const bridge = await frozen;

    if (wantBridge) {
      // the bridge is for the opening of a session only: whether or not it made it this time, it is not tried again
      rec.bridgePending = false;
      this.store.save(sessionId);
    }
    const parts: string[] = [];
    // what the bridge says came from conversations: data, under the same notice as the recall, escaped
    if (bridge.state !== "omitted") parts.push(wrapRelevantMemoriesBlock(bridge.rendering.text));
    if (recall.prependContext) parts.push(recall.prependContext);
    this.logger.info(
      `[${PLUGIN_ID}] turn session=${sessionId} recall=${recall.report.outcome} bridge=${bridge.state === "omitted" ? bridge.why : "injected"} ` +
      `chars=${parts.join("").length} elapsedMs=${budget.elapsedMs()}`,
    );
    return parts.length > 0 ? parts.join("\n\n") : undefined;
  }

  /** Attached images are kept only as long as HeliconSearch may still need to read them to index them. */
  private async pruneUploads(): Promise<void> {
    try {
      const cutoff = Date.now() - UPLOAD_KEEP_MS;
      for (const name of await readdir(this.uploadsDir)) {
        const path = join(this.uploadsDir, name);
        if ((await stat(path)).mtimeMs < cutoff) await unlink(path);
      }
    } catch {
      /* no uploads yet, or a file went away meanwhile */
    }
  }

  /** Saves the images of a message so media_upload can add them (HeliconSearch reads them from disk). */
  private async keepAttachments(sessionId: string, images: ImageAttachment[]): Promise<void> {
    if (!this.media) return;
    try {
      await mkdir(this.uploadsDir, { recursive: true });
      const files = this.attached.get(sessionId) ?? [];
      for (const image of images.slice(0, 10)) {
        const hash = createHash("sha1").update(image.bytes).digest("hex").slice(0, 16);
        const path = join(this.uploadsDir, `${hash}${imageExtension(image.mediaType)}`).split("\\").join("/");
        await writeFile(path, image.bytes);
        if (!files.includes(path)) files.push(path);
      }
      this.attached.set(sessionId, files.slice(-20));
    } catch (err) {
      this.logger.warn(`[${PLUGIN_ID}] could not keep attached images: ${formatHeliconError(err)}`);
    }
  }

  // ── session lifecycle ────────────────────────────────────────────────────

  /** A session was just created by this process: its first turn gets the bridge. */
  onSessionCreated(sessionId: string, agent?: string): void {
    if (!this.allows(agent)) {
      this.excluded.add(sessionId);
      return;
    }
    this.leads.add(sessionId);
    this.known.add(sessionId);
    const rec = this.store.get(sessionId);
    rec.bridgePending = true;
    rec.lastActive = Date.now();
    this.store.save(sessionId);
  }

  /** A session was attached (created or resumed): whatever its history holds beyond the saved cursors is sent. */
  onSessionAttached(sessionId: string, agent?: string): void {
    if (!this.allows(agent)) {
      this.excluded.add(sessionId);
      return;
    }
    this.leads.add(sessionId);
    // A session this adapter has not seen before is only looked at when it takes a turn: attaching a list of
    // old sessions must not archive (and end) all of their histories.
    if (this.store.has(sessionId)) {
      this.known.add(sessionId);
      this.capture.schedule(sessionId);
    }
  }

  /** The session is in use: it is not idle, and if it had been ended it is open again. */
  private touch(sessionId: string): void {
    this.known.add(sessionId);
    this.leads.add(sessionId); // (a `close` that failed leaves the session open: it is followed again)
    const rec = this.store.get(sessionId);
    const now = Date.now();
    // activity is noted at once (the idle sweep reads it from memory), and written to the file at most every
    // few seconds, or at once when it reopens a session that had been ended
    rec.lastActive = now;
    const reopened = rec.ended;
    rec.ended = false;
    if (reopened || now - (this.savedAt.get(sessionId) ?? 0) >= 5_000) {
      this.savedAt.set(sessionId, now);
      this.store.save(sessionId);
    }
  }

  private onJournal(method: string, params: unknown): void {
    if (method === "stream/gap") {
      for (const id of this.capture.sessions()) this.capture.schedule(id);
      return;
    }
    if (method !== "session/event") return;
    const e = params as { type?: unknown; session_id?: unknown } | null;
    if (typeof e?.session_id === "string" && typeof e.type === "string" && TURN_ENDED.has(e.type)) {
      this.running.delete(e.session_id);
    }
    if (e?.type === "turn_completed" && typeof e.session_id === "string" && !this.excluded.has(e.session_id) && this.leads.has(e.session_id)) {
      this.touch(e.session_id); // also makes the session known to the adapter
      this.capture.schedule(e.session_id);
    }
  }

  private readonly ending = new Map<string, Promise<void>>();

  /**
   * Sends what is pending for the session and tells HeliconSearch it is over, which makes it write the
   * session's summary and finish extracting. Once per stretch of activity; never throws.
   */
  private endSession(sessionId: string, waitForExtractionMs: number): Promise<void> {
    const running = this.ending.get(sessionId);
    if (running) return running;
    const work = (async () => {
      try {
        const first = this.store.get(sessionId);
        if (first.ended) return;
        const activity = first.lastActive;
        if (!(await this.capture.flush(sessionId, waitForExtractionMs))) {
          this.logger.warn(`[${PLUGIN_ID}] session ${sessionId} is not ended on HeliconSearch yet: its history is not fully archived`);
          return;
        }
        const archived = this.store.get(sessionId).archiveNext;
        if (archived === 0) {
          // nothing was ever said: there is no HeliconSearch session to end
          this.store.get(sessionId).ended = true;
          this.store.save(sessionId);
          return;
        }
        const hsId = toHeliconSessionId(sessionId);
        const r = await this.client.sessionEnd({
          session_id: hsId,
          agent_id: this.cfg.agentId,
          project_id: this.cfg.projectId,
          source_refs: [{ type: "session", ref_id: hsId }],
          message_count: archived,
        });
        const rec = this.store.get(sessionId);
        if (rec.lastActive > activity) {
          // a turn that came in while this went on has the session in use again: it is not over
          this.logger.info(`[${PLUGIN_ID}] session ${sessionId} was used again while it was being ended; it stays open`);
        } else {
          rec.ended = true;
          this.store.save(sessionId);
        }
        this.logger.info(`[${PLUGIN_ID}] session end status=${r.status} memory_id=${r.memory_id ?? "none"} session=${hsId}`);
        // (the server also summarises on its own; summaryOnSessionEnd off leaves it at that)
        if (r.status === "closed" && !r.memory_id && this.cfg.summaryOnSessionEnd) {
          // the summary is made on the server: ask for it, without waiting
          void this.client
            .sessionSummary({ session_id: hsId, agent_id: this.cfg.agentId, project_id: this.cfg.projectId })
            .then((s) => this.logger.info(`[${PLUGIN_ID}] session summary status=${s.status} memory_id=${s.memory_id ?? "none"}`))
            .catch((err) => this.logger.warn(`[${PLUGIN_ID}] session summary failed: ${formatHeliconError(err)}`));
        }
      } catch (err) {
        this.logger.warn(`[${PLUGIN_ID}] ending session ${sessionId} on HeliconSearch failed: ${formatHeliconError(err)}`);
      }
    })().finally(() => this.ending.delete(sessionId));
    this.ending.set(sessionId, work);
    return work;
  }

  private knownSessions(): string[] {
    return [...new Set([...this.known, ...this.capture.sessions()])];
  }

  /** Waits for `work`, but no longer than `ms`. */
  private static async within(work: Promise<unknown>, ms: number): Promise<void> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    await Promise.race([work, new Promise<void>((resolve) => { timer = setTimeout(resolve, ms); })]).finally(() => clearTimeout(timer));
  }

  /** `session.close()`: sends what is pending and ends the session on HeliconSearch. Bounded; never throws. */
  async onSessionClose(sessionId: string): Promise<void> {
    this.running.delete(sessionId);
    if (this.excluded.delete(sessionId)) return;
    if (this.cfg.syncTranscript) {
      await HeliconAdapter.within(this.endSession(sessionId, this.cfg.closeWaitMs), this.cfg.closeWaitMs + CLOSE_MARGIN_MS);
    } else {
      // (not ended on HeliconSearch, but the last turn is not left behind with the debounce timer)
      await HeliconAdapter.within(this.capture.flush(sessionId, 0).catch(() => undefined), CLOSE_MARGIN_MS);
    }
    this.capture.release(sessionId);
    this.attached.delete(sessionId);
    this.known.delete(sessionId);
    this.leads.delete(sessionId);
    this.savedAt.delete(sessionId);
  }

  /** `session.close()` failed after `onSessionClose` let go of the session (the engine refused it, say because a turn is still running): it is open still, so it is followed again. */
  onSessionCloseFailed(sessionId: string, agent?: string): void {
    if (!this.allows(agent)) {
      this.excluded.add(sessionId);
      return;
    }
    this.touch(sessionId);
  }

  /** Ends the sessions nobody has touched for `idleEndMs`. */
  private sweepIdle(): void {
    const limit = this.cfg.idleEndMs;
    if (!this.cfg.syncTranscript || limit <= 0) return;
    const now = Date.now();
    for (const id of this.knownSessions()) {
      const since = this.running.get(id);
      if (since !== undefined && now - since < RUNNING_CAP_MS) continue; // a turn is in flight
      const rec = this.store.get(id);
      if (!rec.ended && now - rec.lastActive >= limit) void this.endSession(id, 0);
    }
  }

  /**
   * Sessions an earlier run left open (it was killed, or never shut down): archived turns of theirs that
   * were never ended are ended now, a few per start so HeliconSearch's summary queue is not flooded.
   */
  private async endOrphans(): Promise<void> {
    const limit = this.cfg.idleEndMs;
    if (!this.cfg.syncTranscript || limit <= 0) return;
    let ended = 0;
    for (const id of this.store.list()) {
      if (ended >= ORPHANS_PER_START) break;
      // (looked at now, not when the start began: the application may have resumed it since, and a session in
      // use is not an orphan)
      if (this.known.has(id)) continue;
      const rec = this.store.get(id);
      if (rec.ended || rec.archiveNext === 0) {
        this.store.forget(id);
        continue;
      }
      if (Date.now() - rec.lastActive < limit) {
        // a run that died moments ago: the idle sweep ends it once it has been quiet for the limit
        this.known.add(id);
        continue;
      }
      ended++;
      await this.endSession(id, 0);
      if (!this.known.has(id)) this.capture.release(id); // (not one the application took up in the meantime)
    }
  }

  /**
   * `harness.shutdown()`: ends every session this process worked on and has not closed, so HeliconSearch
   * summarises them (an application that exits without closing its sessions gets that anyway). Bounded.
   */
  async shutdown(): Promise<void> {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    if (this.idleTimer) clearInterval(this.idleTimer);
    this.idleTimer = undefined;
    const ids = this.knownSessions();
    const work = ids.map((id) => (this.cfg.endOnShutdown && this.cfg.syncTranscript ? this.endSession(id, 0) : this.capture.flush(id, 0)).catch(() => undefined));
    await HeliconAdapter.within(Promise.all(work), SHUTDOWN_BUDGET_MS);
  }
}
