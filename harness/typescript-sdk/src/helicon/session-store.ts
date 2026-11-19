/**
 * Per-session adapter state that has to survive a restart: where archiving and extraction stand
 * in the engine's history, and whether the session's opening bridge was already shown. Plain JSON
 * files under <stateDir>/helicon-memory/sessions/, written atomically.
 */
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export type SessionRecord = {
  /** ISO time the session was first seen: HeliconSearch's session_datetime */
  startedAt: string;
  /** a new session whose first turn still gets the cross-session bridge */
  bridgePending: boolean;
  /** next history index to archive (turns.v1) */
  archiveNext: number;
  /** next history index to hand to extraction */
  extractNext: number;
  /** ms epoch of the session's last turn (or the start of one): what idle ending goes by */
  lastActive: number;
  /** HeliconSearch was told the session is over; a later turn clears it */
  ended: boolean;
};

const SAFE_ID = /^[A-Za-z0-9_.-]{1,128}$/;

export class SessionStore {
  private readonly dir: string;
  private readonly cache = new Map<string, SessionRecord>();
  /** The last save failed (said once per run of failures, not on every save). */
  private failing = false;

  constructor(root: string, private readonly warn?: (message: string) => void) {
    this.dir = join(root, "sessions");
  }

  private file(sessionId: string): string | undefined {
    return SAFE_ID.test(sessionId) ? join(this.dir, `${sessionId}.json`) : undefined;
  }

  /** Whether the adapter has seen this session before (in this process or a previous one). */
  has(sessionId: string): boolean {
    const f = this.file(sessionId);
    return this.cache.has(sessionId) || (f !== undefined && existsSync(f));
  }

  /** The record, or a fresh one (not saved until `save`). */
  get(sessionId: string): SessionRecord {
    const cached = this.cache.get(sessionId);
    if (cached) return cached;
    let rec: SessionRecord | undefined;
    const f = this.file(sessionId);
    if (f && existsSync(f)) {
      try {
        const raw = JSON.parse(readFileSync(f, "utf8")) as Partial<SessionRecord>;
        rec = {
          startedAt: typeof raw.startedAt === "string" ? raw.startedAt : new Date().toISOString(),
          bridgePending: raw.bridgePending === true,
          archiveNext: Number.isSafeInteger(raw.archiveNext) ? (raw.archiveNext as number) : 0,
          extractNext: Number.isSafeInteger(raw.extractNext) ? (raw.extractNext as number) : 0,
          lastActive: typeof raw.lastActive === "number" ? raw.lastActive : 0,
          ended: raw.ended === true,
        };
      } catch {
        /* a damaged file starts the session's bookkeeping over: the server de-duplicates a resend */
      }
    }
    rec ??= { startedAt: new Date().toISOString(), bridgePending: false, archiveNext: 0, extractNext: 0, lastActive: Date.now(), ended: false };
    this.cache.set(sessionId, rec);
    return rec;
  }

  save(sessionId: string): void {
    const rec = this.cache.get(sessionId);
    const f = this.file(sessionId);
    if (!rec || !f) return;
    try {
      mkdirSync(this.dir, { recursive: true });
      const tmp = `${f}.${process.pid}.tmp`;
      writeFileSync(tmp, JSON.stringify(rec), { mode: 0o600 });
      renameSync(tmp, f);
      this.failing = false;
    } catch (err) {
      // the cursors are an optimisation: without them the next run resends, and the server de-duplicates.
      // A failure is still said, once, so a full disk or a bad permission does not go unnoticed.
      if (!this.failing) {
        this.failing = true;
        this.warn?.(`could not save the progress of session ${sessionId}: ${err instanceof Error ? err.message : String(err)} (it will be sent again after a restart)`);
      }
    }
  }

  /** Ids of the saved sessions (those of earlier runs too). */
  list(): string[] {
    try {
      return readdirSync(this.dir).filter((f) => f.endsWith(".json")).map((f) => f.slice(0, -5));
    } catch {
      return [];
    }
  }

  /** Drops the in-memory copy (the file stays: a later resume continues from it). */
  forget(sessionId: string): void {
    this.cache.delete(sessionId);
  }
}
