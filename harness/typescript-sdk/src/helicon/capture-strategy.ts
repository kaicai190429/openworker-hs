/**
 * CaptureStrategy: pluggable seam for "how to turn an agent_end transcript
 * slice into heliconsearch writes". User explicitly flagged this as the
 * extension point — adding a new strategy later (operator mode, batched,
 * importance-scored, etc.) only requires a new factory plus an enum value
 * in config.captureMode.
 *
 * First release ships:
 *   - DirectStrategy: per-message rule-filtered POST /v1/memory/store
 *   - OperatorStrategy: placeholder; throws unless cfg.captureMode === "operator"
 *     is enabled and VLMService is available on the heliconsearch side.
 */

import { createHash } from "node:crypto";
import type { MemoryConfig } from "./config.js";
import {
  detectCategory,
  extractUserTextContent,
  shouldCapture,
  stripRelevantMemoriesBlock,
} from "./filters.js";
import type { ArchiveRef, ClientLogger, HeliconClient, StoreRequest } from "./helicon-client.js";
import { HeliconApiError } from "./helicon-client.js";
import { renderTranscriptTurn } from "./session-transcript.js";
import { makeSessionRef } from "./source-ref.js";

const PLUGIN_ID = "helicon-memory";

export type CaptureReport = {
  considered: number;
  capturable: number;
  stored: number;
  duplicates: number;
  skipped: number;
  errors: number;
};

export type CaptureContext = {
  messages: unknown[];
  startIndex: number;
  sessionId: string;
  agentId: string;
  projectId: string;
  cfg: MemoryConfig;
  logger: ClientLogger;
  signal?: AbortSignal;
  /** ISO datetime string of when the session started, used for absolute time resolution. */
  sessionStartedAt?: string;
  /**
   * This session's confirmed archive entries by turn_index (from the /session/turn receipts).
   * Operator messages carry the matching ref, so heliconsearch extracts from the archived text
   * and skips turns it already processed. A ref is only attached while the message at that
   * index still is the archived one (see archiveRefFor).
   */
  archiveRefs?: ReadonlyMap<number, ArchiveRef>;
};

export interface CaptureStrategy {
  readonly name: string;
  capture(ctx: CaptureContext): Promise<CaptureReport>;
}

const MAX_CAPTURE_PER_TURN = 3;

// ── Direct: one POST /v1/memory/store per filtered message ───────────────────

export function createDirectStrategy(client: HeliconClient): CaptureStrategy {
  return {
    name: "direct",
    async capture(ctx) {
      const { messages, startIndex, sessionId, agentId, projectId, cfg, logger, signal } = ctx;
      const report: CaptureReport = {
        considered: 0,
        capturable: 0,
        stored: 0,
        duplicates: 0,
        skipped: 0,
        errors: 0,
      };

      let writes = 0;
      for (let index = startIndex; index < messages.length; index++) {
        const message = messages[index];
        const texts = extractUserTextContent(message);
        if (texts.length === 0) {
          continue;
        }
        report.considered++;

        for (const text of texts) {
          logger.info(
            `[${PLUGIN_ID}] auto_capture evaluating msgIdx=${index} len=${text.length} text="${text.slice(0, 120)}"`,
          );
          const captureOpts = {
            ...(cfg.customTriggers ? { customTriggers: cfg.customTriggers } : {}),
            maxChars: cfg.captureMaxChars,
          };
          if (!shouldCapture(text, captureOpts)) {
            logger.info(
              `[${PLUGIN_ID}] auto_capture no trigger match msgIdx=${index} text="${text.slice(0, 80)}"`,
            );
            continue;
          }
          report.capturable++;

          // Limit how many we write per agent_end to avoid runaway batches.
          if (writes >= MAX_CAPTURE_PER_TURN) {
            report.skipped++;
            logger.info(
              `[${PLUGIN_ID}] capture skip msgIdx=${index} (MAX_CAPTURE_PER_TURN=${MAX_CAPTURE_PER_TURN} reached)`,
            );
            continue;
          }

          const category = detectCategory(text);
          const req: StoreRequest = {
            text,
            agent_id: agentId,
            project_id: projectId,
            category,
            scope: "project",
            importance: 0.5,
            resolution: "fact",
            extraction_method: "auto_capture_direct",
            confidence: 0.85,
            source_refs: [makeSessionRef({ sessionId })],
          };

          try {
            const result = await client.store(req, signal ? { signal } : undefined);
            writes++;
            if (result.status === "created") {
              report.stored++;
              logger.info(
                `[${PLUGIN_ID}] capture stored id=${result.id} category=${category} text="${text.slice(0, 60)}"`,
              );
            } else if (result.status === "dedupe") {
              report.duplicates++;
              logger.info(
                `[${PLUGIN_ID}] capture duplicate id=${result.id} text="${text.slice(0, 60)}"`,
              );
            } else {
              report.errors++;
              logger.warn(
                `[${PLUGIN_ID}] capture unexpected status=${result.status} text="${text.slice(0, 60)}"`,
              );
            }
          } catch (err) {
            report.errors++;
            const detail =
              err instanceof HeliconApiError
                ? `status=${err.status} retcode=${err.retcode ?? "?"} ${err.retmsg ?? ""}`
                : String(err);
            logger.warn(`[${PLUGIN_ID}] capture FAILED ${detail} text="${text.slice(0, 60)}"`);
          }
        }
      }
      return report;
    },
  };
}

// ── Operator: placeholder, requires heliconsearch VLMService / THINKING_API_URL ─

function asRecord(v: unknown): Record<string, unknown> | undefined {
  return v && typeof v === "object" && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : undefined;
}

/**
 * Extract plain text from any message role (user or assistant) for the Operator.
 * Handles string content and content-block arrays (text, tool_use, tool_result).
 */
function extractAnyRoleText(message: unknown): { role: string; text: string } | null {
  const msgObj = asRecord(message);
  if (!msgObj) {
    return null;
  }

  const role = typeof msgObj.role === "string" ? msgObj.role : null;
  if (role !== "user" && role !== "assistant") {
    return null;
  }

  const content = msgObj.content;
  let text: string;

  if (typeof content === "string") {
    text = content.trim();
  } else if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const block of content as unknown[]) {
      const b = asRecord(block);
      if (!b) {
        continue;
      }
      if (b.type === "text" && typeof b.text === "string") {
        parts.push(b.text);
      } else if (b.type === "tool_use" && typeof b.name === "string") {
        // Include tool calls as context so the Operator knows what actions were taken
        parts.push(`[tool: ${b.name}]`);
      }
    }
    text = parts.join(" ").trim();
  } else {
    return null;
  }

  // Strip any injected <relevant-memories> recall block so the Operator extracts the
  // user's actual words, not the memories we just recalled back into the prompt.
  const cleaned = stripRelevantMemoriesBlock(text);
  return cleaned ? { role, text: cleaned } : null;
}

/**
 * The archive ref for messages[index], only if that message is still the one archived there.
 *
 * Refs are kept by index, and openclaw rewrites the message array under the same indices —
 * compaction shortens it and shifts everything after, a repaired session file rewrites
 * assistant messages. A ref left over from before would tie a different message to the
 * archived turn: hs would extract the archived (old) text in its place, and skip it
 * entirely if that turn was processed — the new message lost without a trace. So the
 * message is rendered exactly as the archive path renders it and must hash to the ref's
 * content_hash (hs hashes the same text). No match → no ref; hs then matches the message
 * by index and text, and processes it as sent when that fails too.
 */
function archiveRefFor(
  refs: ReadonlyMap<number, ArchiveRef> | undefined,
  message: unknown,
  index: number,
  maxChars: number,
): ArchiveRef | undefined {
  const ref = refs?.get(index);
  if (!ref) {
    return undefined;
  }
  const turn = renderTranscriptTurn(message, index, maxChars);
  if (!turn) {
    return undefined;
  }
  const hash = createHash("sha256").update(turn.content, "utf8").digest("hex");
  return hash === ref.content_hash ? ref : undefined;
}

export function createOperatorStrategy(client: HeliconClient): CaptureStrategy {
  return {
    name: "operator",
    async capture(ctx) {
      const { messages, startIndex, agentId, projectId, sessionId, sessionStartedAt, cfg, logger, signal,
              archiveRefs } = ctx;

      // Build a full user+assistant conversation slice.
      // Including assistant replies is essential for the Operator to correctly
      // infer facts (e.g. "user asked X, assistant confirmed Y" patterns).
      const operatorMessages: Array<{
        role: string;
        content: string;
        turn_index?: number;
        turn_created_at?: string;
        archive_ref?: ArchiveRef;
      }> = [];
      for (let i = startIndex; i < messages.length; i++) {
        const message = messages[i];
        const extracted = extractAnyRoleText(message);
        if (!extracted) {
          continue;
        }
        // Two evidence anchors, both derived from the SAME message object the archival
        // path sees — never from the position inside this payload (the two filters differ,
        // so payload position ≠ session_turn.turn_index and faking it would anchor facts
        // to the wrong turn):
        //   · turn_index — the absolute index into the agent_end message array, which is
        //     exactly what extractTurnsForTranscript() stamps as session_turn.turn_index.
        //     Valid only because both run over the same array in the same hook; it also
        //     lets hs mark this session's raw windows extracted up to this watermark.
        //   · turn_created_at — the message timestamp, derived identically to
        //     session_turn.created_at (new Date(msg.timestamp).toISOString()), kept as a
        //     fallback anchor for turns hs cannot resolve by index.
        // Missing/invalid timestamp → omit (degrades to no anchor, never wrong).
        const rec = asRecord(message);
        const tsMs = typeof rec?.timestamp === "number" ? rec.timestamp : 0;
        const turnCreatedAt = tsMs > 0 ? new Date(tsMs).toISOString() : undefined;
        // Same index space as the archive (see turn_index above). The two paths render a
        // message's text differently (tool calls, block joins), so the ref — not a text match —
        // is what reliably ties this message to its archived turn.
        const archiveRef = archiveRefFor(archiveRefs, message, i, cfg.transcriptMaxCharsPerTurn);
        operatorMessages.push({
          role: extracted.role,
          content: extracted.text,
          turn_index: i,
          ...(turnCreatedAt ? { turn_created_at: turnCreatedAt } : {}),
          ...(archiveRef ? { archive_ref: archiveRef } : {}),
        });
      }

      const report: CaptureReport = {
        considered: operatorMessages.length,
        capturable: operatorMessages.length,
        stored: 0,
        duplicates: 0,
        skipped: 0,
        errors: 0,
      };

      if (operatorMessages.length === 0) {
        return report;
      }

      try {
        const result = await client.add(
          {
            messages: operatorMessages,
            agent_id: agentId,
            project_id: projectId,
            source_refs: [makeSessionRef({ sessionId })],
            session_id: sessionId,
            ...(sessionStartedAt ? { session_datetime: sessionStartedAt } : {}),
            // The auto-capture hook is the one caller that wants the server-side
            // StoreGate; it defaults to off there so that benchmark drivers and the
            // background re-extraction, which post to the same endpoint, keep
            // extracting whatever they send. Sent as an explicit boolean either way.
            store_gate: cfg.storeGate,
          },
          signal ? { signal } : undefined,
        );
        const results = result.operations;

        if (result.archiveMismatched.length > 0) {
          // Our ref for these turns disagrees with the server's archive, so they were left out
          // of the extraction rather than extracted from text the server does not hold.
          logger.warn(
            `[${PLUGIN_ID}] operator capture: ${result.archiveMismatched.length} message(s) do not ` +
            `match the archive and were not extracted (turn_index ${result.archiveMismatched.slice(0, 10).join(",")}) ` +
            `sessionId=${sessionId}`,
          );
        }
        if (result.notArchived && result.notArchived.length > 0) {
          // The server extracts archived turns only (archive first, then extract): these were not
          // in its archive — their archive call failed. Count them as not done so the cursor goes
          // back and they are sent again once archived; extracting them now would extract them a
          // second time when the archive catches up.
          report.errors += result.notArchived.length;
          logger.warn(
            `[${PLUGIN_ID}] operator capture: ${result.notArchived.length} message(s) not in the ` +
            `archive yet, not extracted (turn_index ${result.notArchived.slice(0, 10).join(",")}) ` +
            `sessionId=${sessionId} — sent again after archiving`,
          );
        }
        if (result.archiveConflicts && result.archiveConflicts.length > 0) {
          logger.warn(
            `[${PLUGIN_ID}] operator capture: ${result.archiveConflicts.length} message(s) conflict ` +
            `with the archive and were not extracted (turn_index ` +
            `${result.archiveConflicts.slice(0, 10).join(",")}) sessionId=${sessionId}`,
          );
        }
        if (result.queued) {
          // Durably taken over by the server's ingest ledger (its work unit waits behind this
          // session's earlier work, or these turns are already held): it finishes them in the
          // background. Nothing to resend, nothing failed.
          logger.info(
            `[${PLUGIN_ID}] operator capture: queued on heliconsearch as work unit ` +
            `${result.workUnitId ?? "?"} (sessionId=${sessionId} msgs=${operatorMessages.length})`,
          );
          return report;
        }
        if (result.alreadyProcessed) {
          // A resend (rolled-back cursor, repeated agent_end): the server had processed every
          // turn in it already and extracted nothing again. Nothing to retry.
          logger.info(
            `[${PLUGIN_ID}] operator capture: already processed on heliconsearch ` +
            `(sessionId=${sessionId} msgs=${operatorMessages.length}) — nothing extracted again`,
          );
          return report;
        }
        if (results.length === 0) {
          // Not queued and not a resend, so an empty list means the server settled the slice
          // without writing anything: nothing worth storing, or extraction did not complete
          // (processing=null — its turns are retryable, the server re-extracts them in the
          // background).
          logger.info(
            `[${PLUGIN_ID}] operator capture: no operations returned ` +
            `(sessionId=${sessionId} msgs=${operatorMessages.length} processing=${String(result.processingState)})`,
          );
          return report;
        }

        for (const op of results) {
          if (op.action === "ADD" || op.action === "UPDATE") {
            report.stored++;
          } else if (op.action === "DELETE") {
            report.stored++; // count deletions under stored for visibility
          } else if (op.action === "NOOP") {
            report.skipped++;
          } else {
            report.errors++;
          }
          logger.info(
            `[${PLUGIN_ID}] capture op=${op.action} memoryId=${op.memory_id ?? "?"} ` +
            `status=${op.status ?? "?"} reason="${op.reason ?? ""}"`,
          );
        }
        logger.info(
          `[${PLUGIN_ID}] operator capture done: stored=${report.stored} skipped=${report.skipped} errors=${report.errors} total=${results.length}`,
        );
      } catch (err) {
        report.errors = operatorMessages.length;
        const detail =
          err instanceof HeliconApiError
            ? `status=${err.status} retcode=${err.retcode ?? "?"} ${err.retmsg ?? ""}`
            : String(err);
        logger.warn(`[${PLUGIN_ID}] operator capture FAILED ${detail}`);
      }
      return report;
    },
  };
}
