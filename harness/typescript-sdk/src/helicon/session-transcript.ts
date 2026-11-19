/**
 * Session transcript helpers.
 *
 * Converts openclaw agent_end event.messages into TurnForTranscript[] for
 * POST /v1/memory/session/turn.
 *
 * Design goals:
 *   1. Content fidelity: preserve full text of user/assistant/tool messages,
 *      including complete tool input/output JSON, not placeholder stubs.
 *   2. Timestamp accuracy: use the actual message timestamp from openclaw,
 *      not the time this function runs.
 *   3. maxChars is a safety cap on the final turn text, not a per-block limit.
 *      It applies after all blocks are joined, so short content is never cut.
 */

import type { TurnForTranscript } from "./helicon-client.js";
import { stripRelevantMemoriesBlock } from "./filters.js";

function asRecord(v: unknown): Record<string, unknown> | undefined {
  return v && typeof v === "object" && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : undefined;
}

/**
 * Extract actual message timestamp from an openclaw message object.
 *
 * openclaw stores message.timestamp as a Unix ms integer.
 * Returns ISO 8601 string, falls back to current time only if unavailable.
 */
function extractTimestamp(msg: Record<string, unknown>): string {
  const ts = msg.timestamp;
  if (typeof ts === "number" && ts > 0) {
    return new Date(ts).toISOString();
  }
  if (typeof ts === "string" && ts) {
    return ts;
  }
  return new Date().toISOString();
}

/**
 * Convert openclaw message content to a plain text string.
 *
 * Handles all openclaw content block types, both the Anthropic wire names and
 * openclaw's own normalised names:
 *   - text: returned as-is
 *   - tool_use (Anthropic wire): full name + full input JSON (not a stub)
 *   - toolCall (openclaw normalised): tool NAME only, no arguments — the arg JSON
 *     can be kilobytes and this text feeds the raw semantic windows, where it would
 *     drown the conversation. Mirrors the extraction path (capture-strategy).
 *   - tool_result (Anthropic wire): full output text (not truncated)
 *   - toolResult (openclaw normalised): reached via the role branch below, its
 *     content is a plain text block handled by the text case
 *   - image/document: short placeholder (binary data cannot be represented)
 *
 * Multiple blocks are joined with double newlines to match JSONL readability.
 */

/**
 * openclaw injects a timestamp prefix into user message text before sending
 * to the LLM, e.g. "[Thu 2026-05-21 15:45 GMT+8] actual user text".
 * This prefix is NOT part of the user's original input and should be stripped
 * before archiving so the MD content matches what the user actually typed.
 */
const OPENCLAW_TIMESTAMP_PREFIX_RE = /^\[\w{3} \d{4}-\d{2}-\d{2} \d{2}:\d{2} GMT[+-]\d+\] /;

function stripTimestampPrefix(text: string): string {
  return text.replace(OPENCLAW_TIMESTAMP_PREFIX_RE, "");
}

/**
 * A value as text: a string as it is, a number / boolean as written, an object or array as JSON
 * (String() would make it "[object Object]" — the content lost), nothing as "".
 */
export function valueText(v: unknown): string {
  if (typeof v === "string") {
    return v;
  }
  if (v === null || v === undefined) {
    return "";
  }
  if (typeof v === "number" || typeof v === "boolean" || typeof v === "bigint") {
    return v.toString();
  }
  try {
    return JSON.stringify(v) ?? "";
  } catch {
    return `[${typeof v} not serializable]`;
  }
}

function contentToText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return valueText(content);
  }

  const parts: string[] = [];
  for (const block of content as unknown[]) {
    const b = asRecord(block);
    if (!b) {
      continue;
    }

    if (b.type === "text" && typeof b.text === "string") {
      parts.push(b.text);

    } else if (b.type === "tool_use") {
      const name = typeof b.name === "string" ? b.name : "unknown_tool";
      let inputStr: string;
      try {
        inputStr = JSON.stringify(b.input, null, 2);
      } catch {
        inputStr = b.input === undefined ? "{}" : valueText(b.input);
      }
      parts.push(`[tool_use: ${name}]\n${inputStr}`);

    } else if (b.type === "toolCall") {
      // openclaw's own normalised block name for a tool invocation (the Anthropic
      // wire name is "tool_use"). Without this branch an assistant turn made up of
      // thinking + toolCall renders empty and is archived as the "[no response]"
      // placeholder — verified on a real webui session on 2026-08-17.
      // Deliberately records only the tool NAME, not its arguments: the argument
      // payload can be kilobytes of JSON, and this text is embedded into the raw
      // semantic windows, where it would drown the actual conversation. This mirrors
      // what the extraction path already emits (capture-strategy's extractAnyRoleText).
      const rec = asRecord(b.toolCall) ?? b;
      const name = typeof rec.name === "string" ? rec.name
        : typeof b.name === "string" ? b.name : "unknown_tool";
      parts.push(`[tool: ${name}]`);

    } else if (b.type === "tool_result") {
      const toolUseId = typeof b.tool_use_id === "string" ? ` id=${b.tool_use_id}` : "";
      const rc = b.content;
      let outputStr: string;
      if (typeof rc === "string") {
        outputStr = rc;
      } else if (Array.isArray(rc)) {
        outputStr = (rc as unknown[])
          .map((c) => {
            const cb = asRecord(c);
            return cb?.type === "text" ? valueText(cb.text) : "";
          })
          .filter(Boolean)
          .join("\n");
      } else {
        outputStr = valueText(rc);
      }
      parts.push(`[tool_result${toolUseId}]\n${outputStr}`);

    } else if (b.type === "image") {
      parts.push("[image attachment]");

    } else if (b.type === "document") {
      const docName = typeof b.name === "string" ? ` "${b.name}"` : "";
      parts.push(`[document${docName}]`);
    }
  }

  return parts.join("\n\n");
}

/**
 * Cap the turn text at maxChars UTF-16 code units without cutting a surrogate pair
 * in half: a lone surrogate cannot be encoded as UTF-8, so the archive would either
 * fail to store the turn or store a different text than the one sent.
 */
function capText(text: string, maxChars: number): string {
  if (text.length <= maxChars) {
    return text;
  }
  let end = maxChars;
  const last = text.charCodeAt(end - 1);
  if (last >= 0xd800 && last <= 0xdbff) {
    end -= 1;
  }
  return text.slice(0, end) + "\n…[truncated]";
}

/**
 * The archive turn for one agent_end message (what POST /v1/memory/session/turn
 * stores at this index), or undefined when the message is not archived.
 *
 * Also what ties an Operator message to its archived turn: hs hashes exactly this
 * content, so a message whose rendering hashes to a receipt's content_hash is the
 * message that receipt is for (see capture-strategy archive_ref).
 */
export function renderTranscriptTurn(
  message: unknown,
  index: number,
  maxChars: number,
  runId?: string,
): TurnForTranscript | undefined {
  const msg = asRecord(message);
  if (!msg) {
    return undefined;
  }

  const role = typeof msg.role === "string" ? msg.role : "unknown";
  // "toolResult" is openclaw's own role for a tool's output (the Anthropic wire
  // format instead nests tool_result blocks inside a user message, which is what
  // the bare "tool" entry here was written against). Without it those messages —
  // and the content they carry — were dropped from the archive entirely; a real
  // webui session on 2026-08-17 lost 27/88/217 characters that way.
  if (role !== "user" && role !== "assistant" && role !== "tool" && role !== "toolResult") {
    return undefined;
  }

  // Full content converted to text; cap only at the end.
  // Strip our injected <relevant-memories> recall block (either role) so the archived
  // transcript / raw index matches what was actually said, then remove openclaw's
  // injected timestamp prefix from user messages.
  const converted = stripRelevantMemoriesBlock(contentToText(msg.content));
  const rawText = role === "user" ? stripTimestampPrefix(converted) : converted;
  const text = capText(rawText, maxChars);

  // For assistant turns, keep a placeholder rather than skipping entirely,
  // so turn_index stays in the MD and the sequence is intact.
  const finalText = !text.trim() && role === "assistant" ? "[no response]" : text;

  if (!finalText.trim()) {
    return undefined;
  }

  // Actual message timestamp, not write time
  const createdAt = extractTimestamp(msg);

  const hasToolUse =
    Array.isArray(msg.content) &&
    (msg.content as unknown[]).some((b) => asRecord(b)?.type === "tool_use");
  const hasToolResult =
    Array.isArray(msg.content) &&
    (msg.content as unknown[]).some((b) => asRecord(b)?.type === "tool_result");
  const contentType =
    role === "tool" || hasToolResult ? "tool_result" : hasToolUse ? "tool_use" : "text";

  const meta: Record<string, unknown> = {};
  if (runId) {
    meta["run_id"] = runId;
  }
  if (contentType !== "text") {
    meta["content_type"] = contentType;
  }

  return {
    turn_index: index,
    role,
    content: finalText,
    content_type: contentType,
    created_at: createdAt,
    metadata: Object.keys(meta).length > 0 ? meta : undefined,
  };
}

/**
 * Extract new turns from agent_end messages starting at startIndex.
 *
 * @param messages    Full message array from PluginHookAgentEndEvent
 * @param startIndex  First index not yet stored (from autoCaptureCursor)
 * @param maxChars    Safety cap on final turn text length; applied after
 *                    all content blocks are joined (not per-block truncation)
 * @param runId       openclaw run ID for metadata
 */
export function extractTurnsForTranscript(
  messages: unknown[],
  startIndex: number,
  maxChars: number,
  runId?: string,
): TurnForTranscript[] {
  const turns: TurnForTranscript[] = [];
  for (let i = startIndex; i < messages.length; i++) {
    const turn = renderTranscriptTurn(messages[i], i, maxChars, runId);
    if (turn) {
      turns.push(turn);
    }
  }
  return turns;
}
