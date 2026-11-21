/**
 * Which paths media_upload may hand to HeliconSearch: those inside a folder of `mediaUploadRoots`.
 *
 * HeliconSearch reads (and indexes) whatever path it is given, as its own OS user, so a model that was
 * talked into naming a payroll folder would get it indexed and readable through media_search. The
 * check is text plus the real path of whatever part of the path exists on this machine; it is a guard
 * against the model naming a path, not a sandbox (HeliconSearch opens the path later, and may run on
 * another host).
 */
import { realpathSync } from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";

/** An error that only says the path (or part of it) is not there. */
function isMissing(err: unknown): boolean {
  const code = (err as { code?: unknown } | null)?.code;
  return code === "ENOENT" || code === "ENOTDIR";
}

/**
 * The normalised path with the part that exists replaced by its real path (symlinks followed): the
 * deepest existing ancestor is resolved, the components below it are appended as they are. Undefined
 * when the path is relative, holds a NUL, or something other than "does not exist" went wrong.
 */
function resolveReal(path: string): string | undefined {
  if (!isAbsolute(path) || path.includes("\0")) return undefined;
  const normal = resolve(path); // removes "." and ".." (only a root can still have one)
  const tail: string[] = [];
  let current = normal;
  for (;;) {
    try {
      const real = realpathSync.native(current);
      return tail.length > 0 ? join(real, ...tail.reverse()) : real;
    } catch (err) {
      if (!isMissing(err)) return undefined;
      const parent = dirname(current);
      if (parent === current) return normal; // nothing of it exists here: judged as text
      tail.push(basename(current));
      current = parent;
    }
  }
}

/** The path is the folder itself or lies inside it, by whole path components. */
function isInside(root: string, path: string): boolean {
  const rel = relative(root, path);
  return rel === "" || (rel !== ".." && !rel.startsWith(`..${sep}`) && !isAbsolute(rel));
}

/**
 * Whether `path` (absolute) is one of the `roots` or inside one. Never throws; false when in doubt.
 *
 * A path with a ".." segment is refused outright, on the raw text: normalising it first would collapse
 * `link/..` lexically, while a POSIX kernel follows `link` and then goes up from its target. (A root comes
 * from the operator's configuration and is normalised: a ".." in it is resolved.)
 */
export function isUploadPathAllowed(path: string, roots: readonly string[]): boolean {
  try {
    if (typeof path !== "string" || roots.length === 0) return false;
    if (path.split(/[\\/]/).includes("..")) return false;
    const real = resolveReal(path);
    if (real === undefined) return false;
    return roots.some((root) => {
      const realRoot = typeof root === "string" ? resolveReal(root) : undefined;
      return realRoot !== undefined && isInside(realRoot, real);
    });
  } catch {
    return false;
  }
}
