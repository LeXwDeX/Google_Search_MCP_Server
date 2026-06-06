/**
 * In-memory TTL + LRU cache.
 *
 * Serves two goals at once:
 *  - Performance: identical repeated queries are answered from memory.
 *  - Quota protection: cache hits do not consume the Google daily quota.
 *
 * Eviction policy: entries expire after `ttlMs`; when the store is full the
 * least-recently-used entry is evicted. A Map preserves insertion/access order,
 * which we exploit for O(1) LRU bookkeeping.
 */

interface Entry<V> {
  value: V;
  expiresAt: number; // epoch ms; Infinity means "never expires"
}

export interface CacheStats {
  hits: number;
  misses: number;
  evictions: number;
  expirations: number;
  size: number;
  maxEntries: number;
}

export class TtlLruCache<V> {
  private readonly store = new Map<string, Entry<V>>();
  private hits = 0;
  private misses = 0;
  private evictions = 0;
  private expirations = 0;

  constructor(
    private readonly maxEntries: number,
    private readonly ttlMs: number,
    private readonly now: () => number = Date.now,
  ) {}

  /** Whether caching is effectively disabled. */
  private get disabled(): boolean {
    return this.maxEntries <= 0 || this.ttlMs <= 0;
  }

  get(key: string): V | undefined {
    if (this.disabled) return undefined;
    const entry = this.store.get(key);
    if (entry === undefined) {
      this.misses++;
      return undefined;
    }
    if (entry.expiresAt <= this.now()) {
      this.store.delete(key);
      this.expirations++;
      this.misses++;
      return undefined;
    }
    // Refresh recency: re-insert to move to the most-recent position.
    this.store.delete(key);
    this.store.set(key, entry);
    this.hits++;
    return entry.value;
  }

  set(key: string, value: V): void {
    if (this.disabled) return;
    // Delete first so re-insertion updates recency order.
    this.store.delete(key);
    this.store.set(key, { value, expiresAt: this.now() + this.ttlMs });
    while (this.store.size > this.maxEntries) {
      const oldest = this.store.keys().next().value as string | undefined;
      if (oldest === undefined) break;
      this.store.delete(oldest);
      this.evictions++;
    }
  }

  /** Remove expired entries eagerly (e.g. on a timer). Returns count removed. */
  prune(): number {
    if (this.disabled) return 0;
    const now = this.now();
    let removed = 0;
    for (const [key, entry] of this.store) {
      if (entry.expiresAt <= now) {
        this.store.delete(key);
        this.expirations++;
        removed++;
      }
    }
    return removed;
  }

  clear(): void {
    this.store.clear();
  }

  stats(): CacheStats {
    return {
      hits: this.hits,
      misses: this.misses,
      evictions: this.evictions,
      expirations: this.expirations,
      size: this.store.size,
      maxEntries: this.maxEntries,
    };
  }
}
