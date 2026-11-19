// Test doubles for the Helicon memory adapter: a fake HeliconSearch and a scripted Anthropic Messages server.
import http from "node:http";
import { createHash } from "node:crypto";

const MEMORY_ID = "0123456789abcdef0123456789abcdef";

function json(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

/** A HeliconSearch that records every call. `behaviour.failTurns` makes /session/turn answer 503. */
export async function startFakeHelicon(behaviour = {}) {
  const calls = [];
  const sessions = new Map();
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      let body = {};
      try { body = raw ? JSON.parse(raw) : {}; } catch { /* GET */ }
      const url = new URL(req.url, "http://x");
      calls.push({ method: req.method, path: url.pathname, query: Object.fromEntries(url.searchParams), body, auth: req.headers.authorization });
      const p = url.pathname;
      if (p === "/v1/memory/health") {
        return json(res, 200, { code: 200, data: { qdrant: true, embedding: true, bm25: true, overall: true, recall: { auto_tier_budget_ms: 4000, active_budget_ms: 12000, config_fingerprint: "fp1" } } });
      }
      if (p === "/v1/memory/session/start") {
        sessions.set(body.session_id, []);
        return json(res, 200, { code: 200, data: { session_id: body.session_id, status: "open", started_at: new Date().toISOString() } });
      }
      if (p === "/v1/memory/session/turn") {
        if (behaviour.failTurns) return json(res, 503, { code: 503, message: "down" });
        let results = body.turns.map((t) => ({ turn_index: t.turn_index, role: t.role, sql_status: "stored", turn_id: `turn${t.turn_index}`, content_hash: createHash("sha256").update(t.content, "utf8").digest("hex"), raw_status: "ready" }));
        // behaviour.dropReceipts: the next that many answers carry a receipt for the first turn only
        if ((behaviour.dropReceipts ?? 0) > 0) {
          behaviour.dropReceipts -= 1;
          results = results.slice(0, 1);
        }
        sessions.get(body.session_id)?.push(...body.turns);
        return json(res, 200, { code: 200, data: { archived: results.length, conflicts: 0, errors: 0, partial: false, raw_repair_pending: 0, results } });
      }
      if (p === "/v1/memory/add") {
        // behaviour.addFailures: the next that many extractions are refused (after behaviour.addDelayMs)
        const refuse = (behaviour.addFailures ?? 0) > 0;
        if (refuse) behaviour.addFailures -= 1;
        // behaviour.addRetcodeRefusals: the next that many are answered with HTTP 200 and { retcode, retmsg }
        // (how a tenant check refuses a write)
        if ((behaviour.addRetcodeRefusals ?? 0) > 0) {
          behaviour.addRetcodeRefusals -= 1;
          return json(res, 200, { retcode: 7, retmsg: "tenant refused" });
        }
        return setTimeout(() => (refuse
          ? json(res, 500, { code: 500, message: "extraction down" })
          : json(res, 200, { code: 200, data: [{ action: "ADD", memory_id: "m1", status: "created" }] })), behaviour.addDelayMs ?? 0);
      }
      if (p === "/v1/memory/store") {
        return json(res, 200, { code: 200, data: { memory_id: MEMORY_ID, action: "created" } });
      }
      if (p === "/v1/memory/session_bridge") {
        return json(res, 200, { code: 200, data: {
          protocol: "bridge.v2", is_empty: false, conflict_limit: 5, conflicts: [], procedures: [], category_summaries: [], recent_episodes: [],
          pinned_semantics: [{ id: "b".repeat(32), text: behaviour.bridgeText ?? "User prefers teal.", memory_type: "semantic", category: "preference", scope: "project", agent_id: "a", project_id: "", importance: 0.9, version: "v1" }],
        } });
      }
      if (p === "/v1/memory/query") {
        const facts = behaviour.noRecall ? [] : [{ id: "pkg:abc", claim: "user likes teal", score: 0.9, support_level: "direct", source_memory_ids: [MEMORY_ID] }];
        // behaviour.nulInRecall: a remembered text with a NUL in it (the engine refuses a NUL in the addendum)
        const liked = behaviour.nulInRecall ? "user likes\u0000teal" : "user likes teal";
        const text = facts.length ? `## FACTS\n• ${liked}\n` : "";
        return json(res, 200, { code: 200, grounded_facts: facts, topic_rollups: [], raw_context: [], needs_verification: [], conflicts: [], related_skills: [],
          prompt_text: text,
          prompt_blocks: { protocol: "blocks.v1", blocks: facts.length ? [{ text: "## FACTS\n", ref: { type: "header" } }, { text: `• ${liked}\n`, ref: { type: "memory", memory_id: MEMORY_ID, version: "v1", extras: [] } }] : [] },
        });
      }
      if (p === "/v1/memory/session/end") {
        return json(res, 200, { code: 200, data: { session_id: body.session_id, status: "closed", ended_at: new Date().toISOString() } });
      }
      if (p === "/v1/memory/session/summary") {
        return json(res, 200, { code: 200, data: { session_id: body.session_id, status: "ok" } });
      }
      // ── media: answers {data, retcode, retmsg}; behaviour.mediaFiles are what a search finds ──
      const media = (data, retcode = 0, retmsg = "ok") => json(res, 200, { data, retcode, retmsg });
      if (p === "/v1/kb/list") return media({ kb_list: [{ id: "kb1", name: "photos", filenum: 3, language: "en" }] });
      if (p === "/v1/kb/resolve_default") return media({ kb_id: "kb1", name: "photos", created: false, doc_count: 3 });
      if (p === "/v1/document/upload") {
        if (behaviour.uploadFails) return media({}, 102, "upload failed: no valid file path found");
        return media({ batch_id: "batch1" });
      }
      if (p === "/v1/document/search" || p === "/v1/document/searchbyimage") {
        const files = behaviour.mediaFiles ?? [];
        return media({ data_list: files.map((f, i) => ({ kb_id: body.kb_id, doc_id: `d${i}`, segment_id: 0, file: f.file, image: f.image, info: "", similarity: f.similarity ?? 0.9 - i * 0.1, type: f.type ?? "image", start: f.start ?? 0, end: f.end ?? 0, from: "" })) });
      }
      if (p === "/v1/document/statistics") return media({ total: 3, ready: 2, inprogress: 1, failure: [] });
      if (p === "/v1/document/enumerate") return media({ items: [{ file: "/a.jpg", image_path: "", status: "ready" }], total: 1 });
      json(res, 404, { code: 404, message: "not found" });
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    calls,
    sessions,
    behaviour,
    close: () => new Promise((r) => server.close(r)),
  };
}

/** A scripted Anthropic Messages endpoint. "CALL_TOOL:<name>" in the last user message makes it call that tool. */
export async function startMockModel() {
  let toolCalls = 0;
  const requests = [];
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      let j = {};
      try { j = JSON.parse(raw); } catch { /* ignore */ }
      requests.push(j);
      const last = (j.messages ?? []).at(-1);
      const text = (m) => (typeof m?.content === "string" ? m.content : (m?.content ?? []).map((b) => b.text ?? "").join(""));
      const toolResult = Array.isArray(last?.content) && last.content.some((b) => b.type === "tool_result");
      const sse = (ev, data) => `event: ${ev}\ndata: ${JSON.stringify({ type: ev, ...data })}\n\n`;
      let out = sse("message_start", { message: { id: "m", type: "message", role: "assistant", model: j.model, content: [], stop_reason: null, usage: { input_tokens: 1, output_tokens: 1 } } });
      // "CALL_TOOL:<name>" calls it with {query:"color"}; "CALL_TOOL:<name>|<json>" with that json as the arguments
      const want = !toolResult && /CALL_TOOL:([^\s|]+)(?:\|(\{.*\}))?/.exec(text(last));
      let stop = "end_turn";
      if (want && (j.tools ?? []).some((t) => t.name === want[1])) {
        out += sse("content_block_start", { index: 0, content_block: { type: "tool_use", id: `toolu_${++toolCalls}`, name: want[1], input: {} } });
        out += sse("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: want[2] ?? JSON.stringify({ query: "color" }) } });
        out += sse("content_block_stop", { index: 0 });
        stop = "tool_use";
      } else {
        out += sse("content_block_start", { index: 0, content_block: { type: "text", text: "" } });
        out += sse("content_block_delta", { index: 0, delta: { type: "text_delta", text: toolResult ? "TOOL-DONE" : `ACK: ${text(last).slice(0, 60)}` } });
        out += sse("content_block_stop", { index: 0 });
      }
      out += sse("message_delta", { delta: { stop_reason: stop }, usage: { output_tokens: 2 } }) + sse("message_stop", {});
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.end(out);
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${server.address().port}`, requests, close: () => new Promise((r) => server.close(r)) };
}
