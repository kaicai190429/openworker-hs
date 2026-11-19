// End to end with a REAL model (MiniMax M3.1 Flash), the real engine and a real HeliconSearch. Nothing is scripted:
// the model decides when to call the memory and media tools.
//
//   MINIMAX_API_KEY=... HELICON_LIVE_URL=http://127.0.0.1:5001 HELICON_LIVE_TOKEN=hs_... npm run test:helicon:minimax
//
// MINIMAX_BASE_URL selects the endpoint (default https://api.minimaxi.com). On a machine that reaches the internet only
// through an HTTP proxy the engine cannot call the model itself (its model client uses no proxy), so the test goes through
// a loopback relay that honours https_proxy.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, basename } from "node:path";
import { randomUUID } from "node:crypto";
import { deflateSync } from "node:zlib";
import { startRelay } from "./helicon-relay.mjs";

const url = process.env.HELICON_LIVE_URL;
const token = process.env.HELICON_LIVE_TOKEN;
const modelKey = process.env.MINIMAX_API_KEY;
const MODEL = process.env.MINIMAX_MODEL ?? "MiniMax-M3.1-Flash";
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

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
function colourPng([r, g, b]) {
  const size = 160, row = 1 + size * 3, raw = Buffer.alloc(row * size);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const o = y * row + 1 + x * 3, noise = Math.floor(Math.random() * 6);
      raw[o] = Math.min(255, r + noise); raw[o + 1] = Math.min(255, g + noise); raw[o + 2] = Math.min(255, b + noise);
    }
  }
  const head = Buffer.alloc(13); head.writeUInt32BE(size, 0); head.writeUInt32BE(size, 4); head[8] = 8; head[9] = 2;
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk("IHDR", head), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))]);
}

async function setup(extra = {}) {
  const { Harness } = await import("../dist/index.js");
  process.env.HELICON_LIVE_KEY = token;
  const relay = await startRelay(process.env.MINIMAX_BASE_URL ?? "https://api.minimaxi.com");
  const stateDir = await mkdtemp(join(tmpdir(), "ow-helicon-minimax-"));
  const agentId = `mm-${randomUUID().slice(0, 8)}`;
  await writeFile(join(stateDir, "helicon-memory.json"), JSON.stringify({
    baseUrl: url, apiKey: "${HELICON_LIVE_KEY}", agentId, captureMode: "operator", debug: true, ...extra,
  }));
  const harness = await Harness.start({ stateDir, sandbox: "off" });
  await harness.setModelProvider({ name: "minimax", api: "chat-completions", baseUrl: `${relay.url}/v1`, credentials: { apiKeyEnv: "MINIMAX_API_KEY", storage: "memory" } });
  const agent = harness.createAgent({
    name: `mm-${randomUUID().slice(0, 6)}-v1`,
    instructions: "You are a helpful assistant. Keep answers short. When a tool is needed, call it.",
    defaultModel: { provider: "minimax", name: MODEL },
    tools: [], planning: false, memory: false, scheduling: false, approvals: { mode: "never" },
  });
  const toolCalls = async (session) => (await harness.request("history/read", { session_id: session.id, after: 0 })).items
    .map((i) => i.item).filter((i) => i.type === "function_call").map((i) => ({ name: i.name, args: JSON.parse(i.arguments || "{}") }));
  const hsPost = async (path, body) => (await fetch(`${url}${path}`, { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify(body) })).json();
  return { harness, agent, relay, stateDir, agentId, toolCalls, hsPost, done: async () => { await harness.shutdown(); await relay.close(); } };
}

const skip = (t) => (!url || !token || !modelKey ? (t.skip("needs MINIMAX_API_KEY, HELICON_LIVE_URL and HELICON_LIVE_TOKEN"), true) : false);

test("a real model: what the user said is remembered, recalled in a new session, and searched with the memory tool", async (t) => {
  if (skip(t)) return;
  const x = await setup();
  try {
    const a = await x.agent.newSession();
    const ra = await a.run("Hi! My name is Lin, my cat is called Mochi, and I work as a marine biologist in Qingdao. Just acknowledge.");
    assert.equal(ra.status, "completed", JSON.stringify(ra.providerError ?? ra.failure));
    await a.close(); // ends the session; HeliconSearch extracts on its own

    let facts = [];
    for (let i = 0; i < 60 && facts.length === 0; i++) {
      const q = await x.hsPost("/v1/memory/query", { query: "cat name", agent_id: x.agentId, project_id: "", purpose: "active", top_k: 5, render: "none" });
      facts = (q.grounded_facts ?? []).map((f) => f.claim);
      if (facts.length === 0) await wait(5000);
    }
    assert.ok(facts.some((f) => /mochi/i.test(f)), `HeliconSearch extracted the cat: ${JSON.stringify(facts)}`);

    // a new session answers from the recalled memory, without any tool call
    const b = await x.agent.newSession();
    const rb = await b.run("What is my cat's name, and where do I work?");
    assert.equal(rb.status, "completed", JSON.stringify(rb.providerError ?? rb.failure));
    assert.match(rb.finalOutput ?? "", /mochi/i);
    assert.match(rb.finalOutput ?? "", /qingdao/i);

    // asked to look it up, the model calls the memory tool itself
    const rc = await b.run("Use your memory search tool to look up what my job is, then tell me.");
    assert.equal(rc.status, "completed", JSON.stringify(rc.providerError ?? rc.failure));
    const calls = await x.toolCalls(b);
    assert.ok(calls.some((c) => c.name === "helicon__memory_recall"), `the model used the memory tool: ${JSON.stringify(calls)}`);
    assert.match(rc.finalOutput ?? "", /marine biologist/i);
    await b.close();
  } finally { await x.done(); }
});

test("a real model: it adds an image to the media library and finds it again from a description", async (t) => {
  if (skip(t)) return;
  const x = await setup({ mediaAdminTools: true });
  try {
    const dir = (await mkdtemp(join(tmpdir(), "ow-helicon-mm-img-"))).split("\\").join("/");
    const red = `${dir}/red-${randomUUID().slice(0, 6)}.png`;
    const blue = `${dir}/blue-${randomUUID().slice(0, 6)}.png`;
    await writeFile(red, colourPng([230, 25, 25]));
    await writeFile(blue, colourPng([20, 40, 230]));

    const s = await x.agent.newSession();
    const up = await s.run(`Please add these two image files to my media library: ${red} and ${blue}`);
    assert.equal(up.status, "completed", JSON.stringify(up.providerError ?? up.failure));
    const uploads = (await x.toolCalls(s)).filter((c) => c.name === "helicon__media_upload");
    assert.ok(uploads.length >= 1, "the model called media_upload");
    const sent = uploads.flatMap((c) => c.args.file_names ?? []);
    assert.ok(sent.includes(red) && sent.includes(blue), `both paths were sent: ${JSON.stringify(sent)}`);

    // wait for HeliconSearch to index them
    let stats;
    for (let i = 0; i < 40; i++) {
      stats = await x.hsPost("/v1/document/statistics", {});
      if (stats.data?.inprogress === 0 && stats.data?.total > 0) break;
      await wait(4000);
    }
    assert.equal(stats.data?.inprogress, 0, "indexing finished");

    const found = await s.run("Find the red picture in my media library and tell me its file path.");
    assert.equal(found.status, "completed", JSON.stringify(found.providerError ?? found.failure));
    const searches = (await x.toolCalls(s)).filter((c) => c.name === "helicon__media_search");
    assert.ok(searches.length >= 1, "the model called media_search");
    assert.ok((found.finalOutput ?? "").includes(basename(red)), `the answer names the red file: ${found.finalOutput}`);
    await s.close();
  } finally { await x.done(); }
});

test("a real model: shutting the harness down ends the sessions that were left open, and HeliconSearch summarises them", async (t) => {
  if (skip(t)) return;
  const x = await setup();
  const logFile = join(x.stateDir, "helicon-memory", "adapter.log");
  let summarised = false;
  try {
    const s = await x.agent.newSession();
    const r = await s.run("I am planning a trip to Chengdu next month to eat hotpot with my sister Wei. Please just acknowledge.");
    assert.equal(r.status, "completed", JSON.stringify(r.providerError ?? r.failure));
    // the application never calls session.close(): it just shuts down
    await x.harness.shutdown();
    const log = await readFile(logFile, "utf8");
    assert.match(log, /session end status=closed/, "shutdown ended the open session on HeliconSearch");

    for (let i = 0; i < 60 && !summarised; i++) {
      const bridge = await (await fetch(`${url}/v1/memory/session_bridge?agent_id=${x.agentId}&project_id=&episodic_limit=5`, { headers: { authorization: `Bearer ${token}` } })).json();
      summarised = (bridge.data?.recent_episodes ?? []).length > 0;
      if (!summarised) await wait(5000);
    }
    assert.ok(summarised, "HeliconSearch wrote a summary of the session");
  } finally { await x.relay.close(); }
});
