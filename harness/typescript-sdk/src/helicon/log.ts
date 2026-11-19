/**
 * The adapter's log: warnings and errors always, the rest only with `debug: true`, appended to
 * <stateDir>/helicon-memory/adapter.log. It never writes to stdout/stderr: the host application owns them.
 */
import { appendFileSync, mkdirSync, statSync, renameSync } from "node:fs";
import { dirname } from "node:path";
import type { ClientLogger } from "./helicon-client.js";

const MAX_LOG_BYTES = 5 * 1024 * 1024;

export function createFileLogger(path: string, debug: boolean): ClientLogger {
  let ready = false;
  const write = (level: string, msg: string): void => {
    try {
      if (!ready) {
        mkdirSync(dirname(path), { recursive: true });
        ready = true;
      }
      try {
        if (statSync(path).size > MAX_LOG_BYTES) renameSync(path, `${path}.1`);
      } catch {
        /* no file yet */
      }
      // the log can hold fragments of what users said (debug level, and some warnings): readable by the owner only
      appendFileSync(path, `${new Date().toISOString()} ${level} ${msg}\n`, { mode: 0o600 });
    } catch {
      /* logging must never break the host */
    }
  };
  return {
    info: (m) => { if (debug) write("INFO", m); },
    warn: (m) => write("WARN", m),
    debug: (m) => { if (debug) write("DEBUG", m); },
  };
}
