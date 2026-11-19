/**
 * Chain-of-Evidence source references for HeliconSearch memories.
 *
 * Mirrors the wire shape of heliconsearch's services/memory/source_ref.py
 * so the JSON we POST to /v1/memory/store can be decoded server-side by
 * pydantic without extra translation. Only the shapes openclaw actually
 * produces (turn, manual, merge) are exported as helpers; the other
 * server-known types (file, url, vlm_frame) stay in the union for
 * round-tripping arbitrary refs from heliconsearch responses.
 */

import { randomUUID } from "node:crypto";

export type SourceRefType = "session" | "turn" | "file" | "url" | "vlm_frame" | "manual" | "merge";

export type SourceRef = {
  type: SourceRefType;
  ref_id: string;
  extracted_at: string;
  raw_snippet?: string;
  raw_blob_path?: string;
};

function nowIso(): string {
  return new Date().toISOString();
}

function clampSnippet(text: string | undefined): string | undefined {
  if (!text) {
    return undefined;
  }
  return text.length > 300 ? text.slice(0, 300) : text;
}

/**
 * 构造 session anchor SourceRef（v0.5.1 主路径）。
 *
 * ref_id 格式：`{agent_type}/{session_uuid}`，例如 `openclaw/8838a771-...`
 * 调用方传入的 sessionId 应已经过 toHeliconSessionId() 加前缀处理。
 * L3 原文读取由 adapter 从 agent 原生 session 存储中按 session_id 完成。
 */
export function makeSessionRef(params: {
  sessionId: string;  // 已含 agent_type 前缀，如 "openclaw/8838a771-..."
  extractedAt?: string;
}): SourceRef {
  return {
    type: "session",
    ref_id: params.sessionId,
    extracted_at: params.extractedAt ?? nowIso(),
  };
}

/**
 * 构造 turn 级 SourceRef（向后兼容，新代码优先用 makeSessionRef）。
 *
 * ref_id 格式：`{agent_type}/{session_id}#turn_{turnIndex}`
 */
export function makeTurnRef(params: {
  sessionId: string;
  turnIndex: number;
  snippet?: string;
  extractedAt?: string;
}): SourceRef {
  const snippet = clampSnippet(params.snippet);
  return {
    type: "turn",
    ref_id: `${params.sessionId}#turn_${params.turnIndex}`,
    extracted_at: params.extractedAt ?? nowIso(),
    ...(snippet ? { raw_snippet: snippet } : {}),
  };
}

export function makeManualRef(label = "api_call"): SourceRef {
  return {
    type: "manual",
    ref_id: `${label}_${randomUUID().replace(/-/g, "").slice(0, 8)}`,
    extracted_at: nowIso(),
  };
}

export function makeMergeRef(sourceIds: string[]): SourceRef {
  return {
    type: "merge",
    ref_id: sourceIds.join("+"),
    extracted_at: nowIso(),
  };
}
