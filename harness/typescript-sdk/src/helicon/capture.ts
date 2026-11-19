/**
 * Capture: after each completed turn, archive the new part of the session's history on
 * HeliconSearch (turns.v1) and hand it to fact extraction.
 *
 * It is driven by the engine's journal, not by the application: a `turn_completed` event of any
 * attached session schedules a cycle, so it works the same whether the application used `run()`,
 * `send()` or a stream, and also catches turns the application never awaited. The history is the
 * durable queue — a cycle reads from the cursors saved in the session record, so what could not be
 * delivered (HeliconSearch down, process killed) is simply sent by the next cycle, in the next run.
 *
 * The order is the OpenClaw plugin's: archive first (verbatim text is retrievable at once), then
 * extraction of what was archived — extraction takes tens of seconds to minutes, so it runs
 * detached and never holds up the next archive.
 */
import type { CaptureStrategy } from "./capture-strategy.js";
import type { MemoryConfig } from "./config.js";
import type { ArchiveRef, ClientLogger, HeliconClient } from "./helicon-client.js";
import { HeliconApiError } from "./helicon-client.js";
import { readHistory, toMessages, type HistoryRequest } from "./history.js";
import type { SessionStore } from "./session-store.js";
import { extractTurnsForTranscript } from "./session-transcript.js";

const PLUGIN_ID = "helicon-memory";
const DEBOUNCE_MS = 300;

export const SESSION_PREFIX = "openworker";
export const toHeliconSessionId = (sessionId: string): string => `${SESSION_PREFIX}/${sessionId}`;

type Live = {
  timer?: ReturnType<typeof setTimeout>;
  running?: Promise<void>;
  dirty: boolean;
  /** archived turns' receipts by history index: extraction messages carry them as archive_ref */
  refs: Map<number, ArchiveRef>;
  conflicts: Map<number, string>;
  ownCallIds: Set<string>;
  hsStarted: boolean;
  /** extraction in flight or done up to here (>= the saved extractNext); lowered when one fails */
  extractedTo: number | undefined;
  extraction: Promise<void>;
};

export type CaptureDeps = {
  cfg: MemoryConfig;
  client: HeliconClient;
  strategy: CaptureStrategy;
  store: SessionStore;
  request: HistoryRequest;
  logger: ClientLogger;
  /** model-facing name prefix of the adapter's own tools, e.g. "helicon__" */
  ownToolPrefix: string;
};

function describe(err: unknown): string {
  if (err instanceof HeliconApiError) {
    return `status=${err.status} retcode=${err.retcode ?? "?"} ${err.retmsg ?? ""}`;
  }
  return err instanceof Error ? `${err.name}: ${err.message}` : String(err);
}

export class CaptureEngine {
  private readonly live = new Map<string, Live>();

  constructor(private readonly d: CaptureDeps) {}

  private liveOf(sessionId: string): Live {
    let l = this.live.get(sessionId);
    if (!l) {
      l = {
        dirty: false,
        refs: new Map(),
        conflicts: new Map(),
        ownCallIds: new Set(),
        hsStarted: false,
        extractedTo: undefined,
        extraction: Promise.resolve(),
      };
      this.live.set(sessionId, l);
    }
    return l;
  }

  /** A turn finished: look at the session's history soon (turns arriving together share one cycle). */
  schedule(sessionId: string): void {
    if (!this.d.cfg.autoCapture) return;
    const l = this.liveOf(sessionId);
    l.dirty = true;
    if (l.timer) clearTimeout(l.timer);
    l.timer = setTimeout(() => {
      l.timer = undefined;
      void this.run(sessionId);
    }, DEBOUNCE_MS);
    l.timer.unref?.();
  }

  /** Runs cycles until nothing new is left; concurrent callers share the running one. */
  run(sessionId: string): Promise<void> {
    const l = this.liveOf(sessionId);
    if (l.running) {
      l.dirty = true;
      return l.running;
    }
    l.running = (async () => {
      do {
        l.dirty = false;
        try {
          await this.cycle(sessionId, l);
        } catch (err) {
          this.d.logger.warn(`[${PLUGIN_ID}] capture cycle failed session=${sessionId}: ${describe(err)}`);
        }
      } while (l.dirty);
    })().finally(() => {
      l.running = undefined;
    });
    return l.running;
  }

  /** Archives everything pending now and waits (bounded) for the extraction of it. */
  async flush(sessionId: string, waitForExtractionMs: number): Promise<void> {
    const l = this.liveOf(sessionId);
    if (l.timer) {
      clearTimeout(l.timer);
      l.timer = undefined;
    }
    await this.run(sessionId);
    if (waitForExtractionMs > 0) {
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([
        l.extraction,
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, waitForExtractionMs);
        }),
      ]).finally(() => clearTimeout(timer));
    }
  }

  /** Every session this process has captured for (shutdown flushes them). */
  sessions(): string[] {
    return [...this.live.keys()];
  }

  /** Whether the session has been started on HeliconSearch by this process. */
  private async ensureStarted(sessionId: string, l: Live): Promise<void> {
    if (l.hsStarted) return;
    const rec = this.d.store.get(sessionId);
    await this.d.client.sessionStart({
      session_id: toHeliconSessionId(sessionId),
      agent_id: this.d.cfg.agentId,
      project_id: this.d.cfg.projectId,
      conversation_id: sessionId,
      channel: "openworker",
      session_datetime: rec.startedAt,
    });
    l.hsStarted = true;
  }

  /** `session/start` for a session that is going to be ended without having been archived this run. */
  async start(sessionId: string): Promise<void> {
    await this.ensureStarted(sessionId, this.liveOf(sessionId));
  }

  private async cycle(sessionId: string, l: Live): Promise<void> {
    const { cfg, client, store, logger } = this.d;
    const rec = store.get(sessionId);
    const extractFrom = Math.max(rec.extractNext, l.extractedTo ?? 0);
    const from = Math.min(rec.archiveNext, extractFrom);
    const page = await readHistory(this.d.request, sessionId, from);
    if (page.next <= from) return;

    const messages = toMessages(page, { ownToolPrefix: this.d.ownToolPrefix, ownCallIds: l.ownCallIds, now: Date.now() });
    const hsId = toHeliconSessionId(sessionId);

    // ── 1. archive ──
    if (rec.archiveNext < page.next) {
      const turns = extractTurnsForTranscript(messages, rec.archiveNext, cfg.transcriptMaxCharsPerTurn);
      if (turns.length === 0) {
        rec.archiveNext = page.next; // nothing archivable in this stretch (e.g. only our own tool traffic)
        store.save(sessionId);
      } else {
        await this.ensureStarted(sessionId, l);
        const r = await client.appendTurns({
          session_id: hsId,
          agent_id: cfg.agentId,
          project_id: cfg.projectId,
          conversation_id: sessionId,
          turns,
        });
        if (!Array.isArray(r.results)) {
          throw new Error(`session/turn answered without per-entry receipts session=${hsId}`);
        }
        let retryFrom: number | undefined;
        for (const e of r.results) {
          if ((e.sql_status === "stored" || e.sql_status === "replayed") && e.turn_id && e.content_hash) {
            l.refs.set(e.turn_index, { turn_id: e.turn_id, content_hash: e.content_hash });
          } else if (e.sql_status === "revision_conflict") {
            l.refs.delete(e.turn_index);
            const incoming = e.incoming_hash ?? "";
            if (l.conflicts.get(e.turn_index) !== incoming) {
              l.conflicts.set(e.turn_index, incoming);
              logger.warn(
                `[${PLUGIN_ID}] turn ${e.turn_index} conflicts with the archive (hs keeps ` +
                `${String(e.content_hash).slice(0, 8)}, sent ${incoming.slice(0, 8)}); kept as unresolved session=${hsId}`,
              );
            }
          } else {
            retryFrom = retryFrom === undefined ? e.turn_index : Math.min(retryFrom, e.turn_index);
          }
        }
        rec.archiveNext = retryFrom ?? page.next;
        store.save(sessionId);
        logger.info(
          `[${PLUGIN_ID}] archived turns=${turns.length} from=${from} next=${rec.archiveNext} ` +
          `conflicts=${r.conflicts ?? 0} errors=${r.errors ?? 0} session=${hsId}`,
        );
      }
    }

    // ── 2. extraction of what is archived, detached ──
    const upto = rec.archiveNext;
    if (extractFrom < upto) {
      const startIndex = extractFrom;
      l.extractedTo = upto;
      const slice = messages.slice(0, upto);
      l.extraction = l.extraction.then(() => this.extract(sessionId, l, slice, startIndex, upto));
    }
  }

  private async extract(sessionId: string, l: Live, messages: unknown[], startIndex: number, upto: number): Promise<void> {
    const { cfg, strategy, store, logger } = this.d;
    const rec = store.get(sessionId);
    const failed = (why: string): void => {
      l.extractedTo = Math.min(l.extractedTo ?? startIndex, startIndex);
      logger.warn(`[${PLUGIN_ID}] extraction will be retried from ${startIndex} (${why}) session=${sessionId}`);
    };
    try {
      const report = await strategy.capture({
        messages,
        startIndex,
        sessionId: toHeliconSessionId(sessionId),
        agentId: cfg.agentId,
        projectId: cfg.projectId,
        cfg,
        logger,
        sessionStartedAt: rec.startedAt,
        archiveRefs: l.refs,
      });
      logger.info(
        `[${PLUGIN_ID}] capture done mode=${strategy.name} considered=${report.considered} stored=${report.stored} ` +
        `dedupe=${report.duplicates} skipped=${report.skipped} errors=${report.errors} session=${sessionId}`,
      );
      if (report.errors > 0) {
        failed(`errors=${report.errors}`);
        return;
      }
      // The saved cursor is the end of what is extracted without a gap. When an earlier stretch failed (its
      // extraction is retried from `extractedTo`), this one does not move it past that stretch.
      if (startIndex <= rec.extractNext) {
        rec.extractNext = Math.max(rec.extractNext, upto);
        store.save(sessionId);
      }
    } catch (err) {
      failed(describe(err));
    }
  }

  /** Forgets the in-memory state of a session (its saved cursors stay for a later resume). */
  release(sessionId: string): void {
    const l = this.live.get(sessionId);
    if (l?.timer) clearTimeout(l.timer);
    this.live.delete(sessionId);
    this.d.store.forget(sessionId);
  }
}
