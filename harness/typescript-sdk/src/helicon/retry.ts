/**
 * Minimal transient-error retry used by the Helicon client (replaces the host-provided helper the
 * OpenClaw plugin relied on). Only network-level failures and 5xx/429 responses are retried.
 */
export type TransientRetryConfig = { attempts: number; baseDelayMs: number; maxDelayMs: number };

function isTransient(err: unknown): boolean {
  const e = err as { status?: unknown; name?: string; code?: string } | null;
  if (e && typeof e.status === "number") return e.status >= 500 || e.status === 429;
  if (e?.name === "AbortError") return false; // caller abort or our own deadline: do not retry
  return true; // fetch network failure (TypeError) and similar
}

export async function executeWithRetry<T>(params: {
  operation: () => Promise<T>;
  retry: TransientRetryConfig & { signal?: AbortSignal };
}): Promise<T> {
  const { attempts, baseDelayMs, maxDelayMs, signal } = params.retry;
  let lastError: unknown;
  for (let attempt = 1; attempt <= Math.max(1, attempts); attempt++) {
    try {
      return await params.operation();
    } catch (err) {
      lastError = err;
      if (attempt >= attempts || signal?.aborted || !isTransient(err)) throw err;
      const delay = Math.min(maxDelayMs, baseDelayMs * 2 ** (attempt - 1));
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  }
  throw lastError;
}
