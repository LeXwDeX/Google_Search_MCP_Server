/**
 * Resilient HTTP client for the Google API.
 *
 * Adds the guarantees the native `fetch` lacks for production use:
 *  - Per-attempt timeout via AbortController (no unbounded hangs).
 *  - Retries on transient failures (network errors, 408/425/429/5xx).
 *  - Exponential backoff with full jitter, capped at a max delay.
 *  - Honors the server's `Retry-After` header when present.
 *  - Respects an external abort signal (e.g. graceful shutdown).
 */
import type { Logger } from "./logger.js";

export class HttpError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body: string,
    readonly retryable: boolean,
  ) {
    super(message);
    this.name = "HttpError";
  }
}

export interface HttpClientOptions {
  timeoutMs: number;
  maxRetries: number;
  baseDelayMs: number;
  maxDelayMs: number;
  logger: Logger;
  /** Injectable for tests. */
  fetchImpl?: typeof fetch;
  sleepImpl?: (ms: number, signal?: AbortSignal) => Promise<void>;
}

const RETRYABLE_STATUS = new Set([408, 425, 429, 500, 502, 503, 504]);

const defaultSleep = (ms: number, signal?: AbortSignal): Promise<void> =>
  new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason ?? new Error("aborted"));
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal?.reason ?? new Error("aborted"));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });

/** Parse Retry-After (delta seconds or HTTP date) into milliseconds. */
function parseRetryAfter(header: string | null, now: number): number | undefined {
  if (!header) return undefined;
  const asInt = Number(header);
  if (Number.isFinite(asInt)) return Math.max(0, asInt * 1000);
  const asDate = Date.parse(header);
  if (Number.isFinite(asDate)) return Math.max(0, asDate - now);
  return undefined;
}

export class HttpClient {
  private readonly fetchImpl: typeof fetch;
  private readonly sleep: (ms: number, signal?: AbortSignal) => Promise<void>;

  constructor(private readonly opts: HttpClientOptions) {
    this.fetchImpl = opts.fetchImpl ?? fetch;
    this.sleep = opts.sleepImpl ?? defaultSleep;
  }

  private backoff(attempt: number, retryAfterMs: number | undefined): number {
    if (retryAfterMs !== undefined) {
      return Math.min(retryAfterMs, this.opts.maxDelayMs);
    }
    const exp = this.opts.baseDelayMs * 2 ** attempt;
    const capped = Math.min(exp, this.opts.maxDelayMs);
    // Full jitter: random in [0, capped].
    return Math.floor(Math.random() * capped);
  }

  /**
   * GET a URL and return parsed JSON of type T.
   * `externalSignal` lets the caller cancel across all retries.
   */
  async getJson<T>(url: URL, externalSignal?: AbortSignal): Promise<T> {
    const { maxRetries, timeoutMs, logger } = this.opts;
    let lastErr: unknown;

    for (let attempt = 0; attempt <= maxRetries; attempt++) {
      if (externalSignal?.aborted) {
        throw externalSignal.reason ?? new Error("request aborted");
      }

      const controller = new AbortController();
      const onExternalAbort = () => controller.abort(externalSignal?.reason);
      externalSignal?.addEventListener("abort", onExternalAbort, { once: true });
      const timer = setTimeout(() => controller.abort(new Error(`request timed out after ${timeoutMs}ms`)), timeoutMs);

      try {
        const response = await this.fetchImpl(url, {
          headers: { Accept: "application/json", "User-Agent": "google-search-mcp/0.2" },
          signal: controller.signal,
        });

        if (response.ok) {
          return (await response.json()) as T;
        }

        const body = await response.text().catch(() => "");
        const retryable = RETRYABLE_STATUS.has(response.status);
        const err = new HttpError(
          `Google API HTTP ${response.status} ${response.statusText}`,
          response.status,
          body,
          retryable,
        );

        if (!retryable || attempt === maxRetries) throw err;

        const delay = this.backoff(attempt, parseRetryAfter(response.headers.get("retry-after"), Date.now()));
        logger.warn("retrying after retryable HTTP status", {
          status: response.status,
          attempt: attempt + 1,
          maxRetries,
          delayMs: delay,
        });
        lastErr = err;
        await this.sleep(delay, externalSignal);
      } catch (err) {
        // HttpError that we already decided is non-retryable: rethrow.
        if (err instanceof HttpError && (!err.retryable || attempt === maxRetries)) {
          throw err;
        }
        // External abort: do not retry.
        if (externalSignal?.aborted) throw err;

        // Timeout / network error: retry if attempts remain.
        if (attempt === maxRetries) {
          throw err instanceof Error ? err : new Error(String(err));
        }
        const delay = this.backoff(attempt, undefined);
        logger.warn("retrying after network/timeout error", {
          attempt: attempt + 1,
          maxRetries,
          delayMs: delay,
          error: err instanceof Error ? err.message : String(err),
        });
        lastErr = err;
        await this.sleep(delay, externalSignal);
      } finally {
        clearTimeout(timer);
        externalSignal?.removeEventListener("abort", onExternalAbort);
      }
    }

    throw lastErr instanceof Error ? lastErr : new Error("request failed after retries");
  }
}
