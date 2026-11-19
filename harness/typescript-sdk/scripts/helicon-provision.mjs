#!/usr/bin/env node
// Administrator tool: gives an application its HeliconSearch tenant and writes the configuration it ships with,
// so that its users never create a tenant or see a token. Works on Windows, macOS and Linux (no python, no curl).
//
//   node scripts/helicon-provision.mjs --url http://hs-host:5001 --tenant my-app --out ./helicon-memory.json
//
// It logs in as the HeliconSearch administrator, registers the tenant (or rotates its token when it exists),
// checks that the token works, and writes the configuration. Put that file next to the application's resources
// (<resourcesPath>/helicon-memory.json) or in its state directory.
//
// The administrator password comes from HELICON_TENANT_ADMIN_PW. Without it the factory default of HeliconSearch
// is tried; that default must be changed on any server that is not a throw-away (see register_tenant.sh).
// The token is written into the file (mode 600) unless --env NAME is given, which writes "${NAME}" instead.
import { writeFileSync } from "node:fs";

function args(argv) {
  const out = {};
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) continue;
    const key = a.slice(2);
    const next = argv[i + 1];
    if (next === undefined || next.startsWith("--")) out[key] = true;
    else { out[key] = next; i++; }
  }
  return out;
}

const opt = args(process.argv);
const url = String(opt.url ?? process.env.HELICON_URL ?? "").replace(/\/+$/, "");
const tenant = String(opt.tenant ?? "");
const outFile = String(opt.out ?? "./helicon-memory.json");
if (!url || !tenant || opt.help) {
  console.error("usage: helicon-provision.mjs --url <HeliconSearch URL> --tenant <name> [--out helicon-memory.json] [--agent-id <id>] [--env <VARIABLE>] [--rotate]");
  process.exit(opt.help ? 0 : 2);
}

async function post(path, body, headers = {}) {
  const res = await fetch(url + path, { method: "POST", headers: { "content-type": "application/json", ...headers }, body: JSON.stringify(body) });
  const text = await res.text();
  let json;
  try { json = JSON.parse(text); } catch { json = { raw: text.slice(0, 200) }; }
  return { status: res.status, json };
}

const login = await post("/v1/tenant/admin/login", {
  username: process.env.HELICON_TENANT_ADMIN_USER ?? "admin",
  password: process.env.HELICON_TENANT_ADMIN_PW ?? "helicon-admin",
});
if (login.status !== 200 || !login.json.session) {
  console.error(`administrator login failed (HTTP ${login.status}). Set HELICON_TENANT_ADMIN_PW to the administrator password.`);
  process.exit(1);
}
if (!process.env.HELICON_TENANT_ADMIN_PW) {
  console.error("warning: logged in with HeliconSearch's factory administrator password. Change it before this server holds anything real.");
}
const admin = { "X-Admin-Session": login.json.session };

let issued = opt.rotate ? { status: 409, json: {} } : await post("/v1/tenant/register", { username: tenant, level: 0 }, admin);
if (issued.status === 409) {
  issued = await post("/v1/tenant/rotate", { username: tenant }, admin);
  console.error(`tenant ${tenant} exists: a new token was issued (the earlier ones stay valid until revoked)`);
} else if (issued.status === 200) {
  console.error(`tenant ${tenant} created`);
}
const token = issued.json.token;
if (issued.status !== 200 || typeof token !== "string") {
  console.error(`could not obtain a token for ${tenant} (HTTP ${issued.status}): ${JSON.stringify(issued.json).slice(0, 200)}`);
  process.exit(1);
}

const who = await fetch(url + "/v1/tenant/whoami", { headers: { authorization: `Bearer ${token}` } });
if (!who.ok) {
  console.error(`the new token was refused by whoami (HTTP ${who.status}); the tenant exists, do not register it again`);
  process.exit(1);
}

const config = {
  baseUrl: url,
  apiKey: typeof opt.env === "string" ? "${" + opt.env + "}" : token,
  agentId: typeof opt["agent-id"] === "string" ? opt["agent-id"] : tenant,
  captureMode: "operator",
};
writeFileSync(outFile, JSON.stringify(config, null, 2) + "\n", { mode: 0o600 });
console.error(`wrote ${outFile}${typeof opt.env === "string" ? ` (the token is read from the environment variable ${opt.env}; it is not in the file)` : " (it contains the token: ship it with the application, keep it out of source control)"}`);
if (typeof opt.env === "string") console.log(token);
