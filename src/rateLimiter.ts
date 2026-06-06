/**
 * Rate limiting and daily quota enforcement.
 *
 * Two independent guards:
 *  1. A per-second token bucket that smooths burst traffic. When no token is
 *     available the caller *waits* (up to a configurable ceiling) instead of
 *     failing immediately — this turns transient bursts into slight latency
 *     rather than errors.
 *  2. A hard daily quota that mirrors the Google free tier (100/day). When the
 *     quota is exhausted there is nothing to wait for within a reasonable
 *     window, so we fail fast with a typed error.
 *
 * Acquisitions are serialized through a promise chain so concurrent callers
 * cannot race past the limits.
 */

export class QuotaExceededError extends Error {
  constructor(
    message: string,
    readonly used: number,
    readonly limit: number,
    readonly resetAt: number,
  ) {
    super(message);
    this.name = "QuotaExceededError";
  }
}

export class RateLimitTimeoutError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RateLimitTimeoutError";
  }
}

const sleep = (ms: number, signal?: AbortSignal): Promise<void> =>
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

function startOfUtcDay(now: number): number {
  return new Date(now).setUTCHours(0, 0, 0, 0);
}

export interface RateLimiterStats {
  dailyUsed: number;
  dailyLimit: number;
  dailyResetAt: number;
  perSecondLimit: number;
}

export class RateLimiter {
  private tokens: number;
  private lastRefill: number;
  private dailyUsed = 0;
  private dayStart: number;
  /** Serializes acquire() so concurrent callers queue deterministically. */
  private chain: Promise<void> = Promise.resolve();

  constructor(
    private readonly perSecondLimit: number,
    private readonly dailyLimit: number,
    private readonly maxWaitMs: number,
    private readonly now: () => number = Date.now,
  ) {
    this.tokens = perSecondLimit;
    this.lastRefill = now();
    this.dayStart = startOfUtcDay(this.lastRefill);
  }

  private refillTokens(): void {
    const now = this.now();
    const elapsed = now - this.lastRefill;
    if (elapsed <= 0) return;
    const refill = (elapsed / 1000) * this.perSecondLimit;
    if (refill >= 1) {
      this.tokens = Math.min(this.perSecondLimit, this.tokens + Math.floor(refill));
      this.lastRefill = now;
    }
  }

  private rollDayIfNeeded(): void {
    const today = startOfUtcDay(this.now());
    if (today > this.dayStart) {
      this.dayStart = today;
      this.dailyUsed = 0;
    }
  }

  /**
   * Acquire one unit of permission. Resolves once allowed; rejects with
   * QuotaExceededError when the daily quota is gone, or RateLimitTimeoutError
   * when the per-second wait would exceed maxWaitMs.
   */
  async acquire(signal?: AbortSignal): Promise<void> {
    // Serialize: each caller waits for the previous acquisition to settle.
    const run = this.chain.then(() => this.doAcquire(signal));
    // Keep the chain alive regardless of individual outcome.
    this.chain = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  private async doAcquire(signal?: AbortSignal): Promise<void> {
    this.rollDayIfNeeded();
    if (this.dailyUsed >= this.dailyLimit) {
      const resetAt = this.dayStart + 86_400_000;
      throw new QuotaExceededError(
        `Daily quota exhausted (${this.dailyUsed}/${this.dailyLimit}). Resets at ${new Date(resetAt).toISOString()}.`,
        this.dailyUsed,
        this.dailyLimit,
        resetAt,
      );
    }

    const deadline = this.now() + this.maxWaitMs;
    // Wait for a per-second token to become available.
    for (;;) {
      this.refillTokens();
      if (this.tokens >= 1) {
        this.tokens -= 1;
        this.dailyUsed += 1;
        return;
      }
      // Time until the next whole token is produced.
      const msPerToken = 1000 / this.perSecondLimit;
      let wait = Math.ceil(msPerToken);
      const remaining = deadline - this.now();
      if (remaining <= 0) {
        throw new RateLimitTimeoutError(
          `Rate limit wait exceeded ${this.maxWaitMs}ms (per-second limit ${this.perSecondLimit}).`,
        );
      }
      wait = Math.min(wait, remaining);
      await sleep(wait, signal);
    }
  }

  stats(): RateLimiterStats {
    this.rollDayIfNeeded();
    return {
      dailyUsed: this.dailyUsed,
      dailyLimit: this.dailyLimit,
      dailyResetAt: this.dayStart + 86_400_000,
      perSecondLimit: this.perSecondLimit,
    };
  }
}
