// The memory adapter against the real engine, with a fake HeliconSearch and a scripted model on loopback: where the recall
// goes in what the model is sent (`recallPlacement`), and what HeliconSearch is handed. No keys, no network beyond loopback.
// Skipped when no engine runtime is installed.
//
//   npm run test:helicon:placement
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startFakeHelicon, startMockModel } from "./helicon-fixtures.mjs";

const systemOf = (req) => (typeof req.system === "string" ? req.system : JSON.stringify(req.system));
const userTexts = (req) => (req.messages ?? [])
  .filter((m) => m.role === "user")
  .map((m) => (typeof m.content === "string" ? m.content : (m.content ?? []).map((b) => b.text ?? "").join("")));

async function start(t, extra) {
  const { Harness } = await import("../dist/index.js");
  const hs = await startFakeHelicon();
  const model = await startMockModel();
  const stateDir = await mkdtemp(join(tmpdir(), "ow-helicon-placement-"));
  await writeFile(join(stateDir, "helicon-memory.json"), JSON.stringify({
    baseUrl: hs.url, apiKey: "hs_test", agentId: "tester", captureMode: "operator", closeWaitMs: 0, ...extra,
  }));
  let harness;
  try {
    harness = await Harness.start({ stateDir, sandbox: "off" });
  } catch (err) {
    await hs.close();
    await model.close();
    t.skip(`no engine runtime: ${err.message}`);
    return undefined;
  }
  await harness.setModelProvider({ name: "mockmodel", api: "anthropic", baseUrl: model.url, credentials: { apiKey: "sk-mock-key-0001", storage: "memory" } });
  const agent = harness.createAgent({ name: "helicon-placement-v1", instructions: "You are a test agent.", defaultModel: { provider: "mockmodel", name: "claude-test" }, tools: [], planning: false, memory: false, scheduling: false, approvals: { mode: "never" } });
  const done = async () => { await harness.shutdown(); await model.close(); await hs.close(); };
  return { hs, model, agent, done };
}

test("with recallPlacement user the model gets the recall in the user's message, session.send and session.run alike", async (t) => {
  const s = await start(t, { recallPlacement: "user" });
  if (!s) return;
  try {
    const session = await s.agent.newSession();
    await session.run("What tea do I like?");
    const first = s.model.requests[0];
    assert.doesNotMatch(systemOf(first), /Application instructions for this turn|user likes teal|prefers teal/, "nothing recalled in the instructions");
    const sent = userTexts(first).at(-1);
    assert.match(sent, /^<relevant-memories>\n/);
    assert.match(sent, /user likes teal/);
    assert.match(sent, /跨会话记忆/, "the first turn's cross-session summary is in the same prefix");
    assert.ok(sent.endsWith("</relevant-memories>\n\nWhat tea do I like?"), sent);

    // send (session/send) the second turn: the history keeps the first message as the model saw it
    const run = await session.send("And coffee?");
    await run.result();
    const second = s.model.requests.at(-1);
    const users = userTexts(second);
    assert.ok(users[0].endsWith("</relevant-memories>\n\nWhat tea do I like?"), "the first message is in the history as it was sent");
    assert.ok(users.at(-1).endsWith("</relevant-memories>\n\nAnd coffee?"), users.at(-1));
    assert.equal(users.filter((x) => x.includes("跨会话记忆")).length, 1, "the cross-session summary only on the first turn");

    // HeliconSearch is asked with the user's words alone and is archived without the recall
    const queries = s.hs.calls.filter((c) => c.path === "/v1/memory/query");
    assert.equal(queries[0].body.query, "What tea do I like?");
    assert.equal(queries[1].body.query, "And coffee?");
    await session.close();
    const turn = s.hs.calls.find((c) => c.path === "/v1/memory/session/turn");
    assert.equal(turn.body.turns[0].content, "What tea do I like?");
    // (the scripted model echoes the start of the message it was sent, so only what the user said is checked)
    const said = s.hs.calls.filter((c) => c.path === "/v1/memory/session/turn").flatMap((c) => c.body.turns).filter((x) => x.role === "user");
    assert.deepEqual(said.map((x) => x.content), ["What tea do I like?", "And coffee?"]);
  } finally { await s.done(); }
});

test("with recallPlacement system the recall stays in the turn's instructions and the user's message is the user's own", async (t) => {
  const s = await start(t, { recallPlacement: "system" });
  if (!s) return;
  try {
    const session = await s.agent.newSession();
    await session.run("What tea do I like?");
    const first = s.model.requests[0];
    assert.match(systemOf(first), /<relevant-memories>/);
    assert.match(systemOf(first), /user likes teal/);
    assert.deepEqual(userTexts(first), ["What tea do I like?"]);
    await session.close();
  } finally { await s.done(); }
});

test("without the option the recall is in front of the user's message", async (t) => {
  const s = await start(t, {});
  if (!s) return;
  try {
    const session = await s.agent.newSession();
    await session.run("What tea do I like?");
    const first = s.model.requests[0];
    assert.doesNotMatch(systemOf(first), /user likes teal/);
    assert.ok(userTexts(first).at(-1).endsWith("</relevant-memories>\n\nWhat tea do I like?"));
    await session.close();
  } finally { await s.done(); }
});
