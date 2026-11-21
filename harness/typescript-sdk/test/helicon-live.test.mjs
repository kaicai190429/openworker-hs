// Runs against a real HeliconSearch and the real harness runtime (the model is scripted).
//   HELICON_LIVE_URL=http://127.0.0.1:5001 HELICON_LIVE_TOKEN=hs_... npm run test:helicon:live
// Skipped without those variables. It writes to the tenant the token belongs to, under a unique agent id.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { startMockModel } from "./helicon-fixtures.mjs";

const url = process.env.HELICON_LIVE_URL;
const token = process.env.HELICON_LIVE_TOKEN;

test("a fact told in one session is recalled in the next, through the real engine and HeliconSearch", async (t) => {
  if (!url || !token) return t.skip("HELICON_LIVE_URL / HELICON_LIVE_TOKEN not set");
  const { Harness } = await import("../dist/index.js");
  process.env.HELICON_LIVE_KEY = token;
  const model = await startMockModel();
  const stateDir = await mkdtemp(join(tmpdir(), "ow-helicon-live-"));
  const agentId = `live-${randomUUID().slice(0, 8)}`;
  await writeFile(join(stateDir, "helicon-memory.json"), JSON.stringify({
    baseUrl: url, apiKey: "${HELICON_LIVE_KEY}", agentId, captureMode: "operator", closeWaitMs: 0,
  }));
  const harness = await Harness.start({ stateDir, sandbox: "off" });
  try {
    await harness.setModelProvider({ name: "mockmodel", api: "anthropic", baseUrl: model.url, credentials: { apiKey: "sk-mock-key-0001", storage: "memory" } });
    const agent = harness.createAgent({ name: "helicon-live-v1", instructions: "You are a test agent.", defaultModel: { provider: "mockmodel", name: "claude-test" }, tools: [], planning: false, memory: false, scheduling: false, approvals: { mode: "never" } });
    const systemOf = (req) => (typeof req.system === "string" ? req.system : JSON.stringify(req.system));

    const a = await agent.newSession();
    await a.run("My favourite colour is teal and I live in Chengdu.");
    await a.close(); // archives and ends the session; HeliconSearch extracts the facts on its own

    // how long that takes is up to the server (extraction is queued there): wait until the fact is recalled
    let recalled = false;
    for (let i = 0; i < 60 && !recalled; i++) {
      const r = await fetch(`${url}/v1/memory/query`, {
        method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify({ query: "favourite colour", agent_id: agentId, project_id: "", purpose: "active", top_k: 5, render: "none" }),
      });
      const j = await r.json();
      recalled = (j.grounded_facts ?? []).some((f) => /teal/i.test(f.claim));
      if (!recalled) await new Promise((res) => setTimeout(res, 5000));
    }
    assert.ok(recalled, "HeliconSearch extracted the fact after the session ended");

    const b = await agent.newSession();
    const n = model.requests.length;
    await b.run("What is my favourite colour?");
    const system = systemOf(model.requests[n]);
    assert.match(system, /<relevant-memories>/);
    assert.match(system, /teal/i);

    const m = model.requests.length;
    await b.run("CALL_TOOL:helicon__memory_recall");
    const toolResult = model.requests[m + 1].messages.at(-1);
    assert.match(JSON.stringify(toolResult), /teal/i, "the recall tool answers from the same memory");
    await b.close();
  } finally {
    await harness.shutdown();
    await model.close();
  }
});

// ── media: attach an image, add it to the library, find it again ───────────────

import { deflateSync } from "node:zlib";
import { mkdir } from "node:fs/promises";

function crc32(buf) {
  let c, crc = ~0;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = (crc >>> 8) ^ c;
  }
  return ~crc >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}
/** A 160x160 PNG of one colour with a little random noise, so no two runs hold the same file. */
function colourPng([r, g, b]) {
  const size = 160, row = 1 + size * 3, raw = Buffer.alloc(row * size);
  for (let y = 0; y < size; y++) {
    raw[y * row] = 0;
    for (let x = 0; x < size; x++) {
      const o = y * row + 1 + x * 3, noise = Math.floor(Math.random() * 6);
      raw[o] = Math.min(255, r + noise); raw[o + 1] = Math.min(255, g + noise); raw[o + 2] = Math.min(255, b + noise);
    }
  }
  const head = Buffer.alloc(13); head.writeUInt32BE(size, 0); head.writeUInt32BE(size, 4); head[8] = 8; head[9] = 2;
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk("IHDR", head), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))]);
}

test("media: an attached image is added to the library, found by a text query, and shown to the model", async (t) => {
  if (!url || !token) return t.skip("HELICON_LIVE_URL / HELICON_LIVE_TOKEN not set");
  const { Harness, Attachment } = await import("../dist/index.js");
  process.env.HELICON_LIVE_KEY = token;
  const model = await startMockModel();
  const stateDir = await mkdtemp(join(tmpdir(), "ow-helicon-live-media-"));
  const srcDir = await mkdtemp(join(tmpdir(), "ow-helicon-live-img-"));
  await mkdir(srcDir, { recursive: true });
  // media_upload adds a named path only from a folder the configuration allows
  await writeFile(join(stateDir, "helicon-memory.json"), JSON.stringify({
    baseUrl: url, apiKey: "${HELICON_LIVE_KEY}", agentId: `live-${randomUUID().slice(0, 8)}`, captureMode: "direct", mediaInlineImages: true, mediaAdminTools: true,
    mediaUploadRoots: [srcDir],
  }));
  const bluePath = join(srcDir, `blue-${randomUUID().slice(0, 6)}.png`).split("\\").join("/");
  await writeFile(bluePath, colourPng([20, 40, 230]));

  const harness = await Harness.start({ stateDir, sandbox: "off" });
  try {
    await harness.setModelProvider({ name: "mockmodel", api: "anthropic", baseUrl: model.url, credentials: { apiKey: "sk-mock-key-0001", storage: "memory" } });
    const agent = harness.createAgent({ name: "helicon-live-media-v1", instructions: "You are a test agent.", defaultModel: { provider: "mockmodel", name: "claude-test" }, tools: [], planning: false, memory: false, scheduling: false, approvals: { mode: "never" } });
    const session = await agent.newSession();
    const outputs = async () => (await harness.request("history/read", { session_id: session.id, after: 0 })).items
      .map((i) => i.item).filter((i) => i.type === "function_call_output")
      .map((i) => { const o = JSON.parse(i.output); return o.structuredContent ?? o.content?.[0]?.text; });
    const call = async (name, args, attachments) => {
      const text = `CALL_TOOL:helicon__${name}|${JSON.stringify(args)}`;
      const r = attachments ? await session.run(text, attachments) : await session.run(text);
      assert.equal(r.status, "completed", r.failure);
      return (await outputs()).at(-1);
    };

    // the user attaches a red picture and asks for it to be kept: no path is given
    const red = await call("media_upload", {}, [Attachment.fromBytes({ name: "red.png", mediaType: "image/png", data: colourPng([230, 25, 25]) })]);
    assert.ok(red.batchId, "HeliconSearch accepted the batch");
    assert.match(red.files[0], /helicon-memory\/uploads\/[0-9a-f]{16}\.png$/);
    // and a second one by its path
    const blue = await call("media_upload", { file_names: [bluePath] });
    assert.ok(blue.batchId);

    // wait until HeliconSearch has indexed both
    let stats;
    for (let i = 0; i < 40; i++) {
      stats = await call("media_statistics", {});
      if (stats.inProgress === 0) break;
      await new Promise((r) => setTimeout(r, 4000));
    }
    assert.equal(stats.inProgress, 0, "indexing finished");
    assert.equal(stats.failures, 0);

    const before = model.requests.length;
    // each picture is found by its own description (results far weaker than the best are dropped, so the other colour need not appear)
    const found = await call("media_search", { query: "a solid red image", max_results: 50, lowest_similarity: 0 });
    assert.ok(found.results.some((r) => r.file === red.files[0]), "the attached red image is found by a red query");
    const foundBlue = await call("media_search", { query: "a solid blue image", max_results: 50, lowest_similarity: 0 });
    assert.ok(foundBlue.results.some((r) => r.file === bluePath), "the image added by path is found by a blue query");

    // the best hit was also put in front of the model as a picture
    const afterSearch = JSON.stringify(model.requests.slice(before).at(-1).messages);
    assert.match(afterSearch, /"type":"image"/);
    assert.equal(found.shownToModel, found.results[0].file);
    assert.ok((await call("media_libraries", {})).libraries.length >= 1);
  } finally {
    await harness.shutdown();
    await model.close();
  }
});
