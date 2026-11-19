/**
 * Where the Helicon memory adapter's configuration comes from, and whether it is on.
 *
 * Nothing changes for an application that has no such file: the adapter is only attached when a
 * configuration is found and enabled. Lookup order:
 *   1. the file named by OPENWORKER_HELICON_CONFIG
 *   2. <stateDir>/helicon-memory.json
 *   3. <resourcesPath>/helicon-memory.json (a packaged Electron application's own resources)
 *
 * The file holds the options of `MemoryConfig` (config.ts) plus `"enabled"` (default true). The
 * credential is not written into it: `"apiKey": "${HELICON_TENANT_TOKEN}"` is read from the
 * environment of the host process.
 */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { memoryConfigSchema, UNCONFIGURED_API_KEY, type MemoryConfig } from "./config.js";

export const CONFIG_ENV = "OPENWORKER_HELICON_CONFIG";
export const CONFIG_FILE = "helicon-memory.json";

export type ConfigLoad =
  | { status: "absent" }
  | { status: "disabled"; path: string; reason: string }
  | { status: "invalid"; path: string; reason: string }
  | { status: "enabled"; path: string; cfg: MemoryConfig };

export function loadHeliconConfig(stateDir: string, env: NodeJS.ProcessEnv = process.env): ConfigLoad {
  const fromEnv = env[CONFIG_ENV]?.trim();
  // a packaged desktop application ships the file in its resources, so its users configure nothing
  const resources = (process as NodeJS.Process & { resourcesPath?: string }).resourcesPath;
  const candidates = [join(stateDir, CONFIG_FILE), ...(resources ? [join(resources, CONFIG_FILE)] : [])];
  const path = fromEnv || candidates.find((p) => existsSync(p)) || candidates[0]!;
  if (!existsSync(path)) {
    // an explicit path that is missing is a mistake worth reporting; the default one is just "off"
    return fromEnv ? { status: "invalid", path, reason: "file not found" } : { status: "absent" };
  }
  let raw: unknown;
  try {
    raw = JSON.parse(readFileSync(path, "utf8"));
  } catch (err) {
    return { status: "invalid", path, reason: `not valid JSON: ${err instanceof Error ? err.message : String(err)}` };
  }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return { status: "invalid", path, reason: "expected a JSON object" };
  }
  const { enabled, ...rest } = raw as Record<string, unknown>;
  if (enabled === false) {
    return { status: "disabled", path, reason: "enabled=false" };
  }
  if (enabled !== undefined && enabled !== true) {
    return { status: "invalid", path, reason: "enabled must be true or false" };
  }
  try {
    const cfg = memoryConfigSchema.parse(rest);
    if (cfg.apiKey === UNCONFIGURED_API_KEY) {
      // The OpenClaw plugin sends a placeholder so a middlebox can substitute the real token. A
      // harness application has no such middlebox, so a missing token is a configuration error.
      return { status: "invalid", path, reason: "apiKey is missing, or the environment variable it names is not set" };
    }
    return { status: "enabled", path, cfg };
  } catch (err) {
    return { status: "invalid", path, reason: err instanceof Error ? err.message : String(err) };
  }
}
