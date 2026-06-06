/**
 * Google Custom Search core.
 *
 * Single code path for both web and image search. Responsibilities:
 *  - Validate and clamp inputs to the API's hard limits.
 *  - Serve from cache when possible (and never burn quota on a cache hit).
 *  - Coalesce identical concurrent requests into one upstream call.
 *  - Enforce rate limits / quota before hitting the network.
 *  - Format results into a stable, human-readable text block.
 */
import { z } from "zod";
import { GOOGLE_LIMITS, type Config } from "./config.js";
import type { Logger } from "./logger.js";
import type { TtlLruCache } from "./cache.js";
import type { RateLimiter } from "./rateLimiter.js";
import type { HttpClient } from "./http.js";

export class InvalidInputError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "InvalidInputError";
  }
}

const WebArgsSchema = z.object({
  query: z.string().trim().min(1, "query must be a non-empty string").max(2048),
  count: z.coerce.number().int().catch(5).default(5),
  start: z.coerce.number().int().catch(1).default(1),
  site: z.string().trim().max(255).optional().default(""),
});

const ImageArgsSchema = z.object({
  query: z.string().trim().min(1, "query must be a non-empty string").max(2048),
  count: z.coerce.number().int().catch(5).default(5),
  start: z.coerce.number().int().catch(1).default(1),
});

export type SearchType = "web" | "image";

interface GoogleItemWeb {
  title?: string;
  link?: string;
  snippet?: string;
  displayLink?: string;
}

interface GoogleItemImage {
  title?: string;
  link?: string;
  snippet?: string;
  image?: { thumbnailLink?: string; height?: number; width?: number; contextLink?: string };
}

interface GoogleResponse<T> {
  kind?: string;
  items?: T[];
  searchInformation?: { totalResults?: string; searchTime?: number };
  error?: { code: number; message: string };
}

/** Clamp count/start into the API's valid ranges. */
function clampPaging(count: number, start: number): { count: number; start: number } {
  let c = Number.isFinite(count) ? Math.trunc(count) : 5;
  let s = Number.isFinite(start) ? Math.trunc(start) : 1;
  c = Math.max(1, Math.min(GOOGLE_LIMITS.maxCount, c));
  s = Math.max(1, Math.min(GOOGLE_LIMITS.maxStart, s));
  // start + num must not exceed 101.
  if (s + c > GOOGLE_LIMITS.maxStartPlusNum) {
    c = Math.max(1, GOOGLE_LIMITS.maxStartPlusNum - s);
  }
  return { count: c, start: s };
}

export class GoogleSearch {
  private readonly inflight = new Map<string, Promise<string>>();

  constructor(
    private readonly cfg: Config,
    private readonly http: HttpClient,
    private readonly limiter: RateLimiter,
    private readonly cache: TtlLruCache<string>,
    private readonly logger: Logger,
  ) {}

  /** Parse + validate raw tool arguments for the web search tool. */
  parseWebArgs(args: unknown): { query: string; count: number; start: number; site: string } {
    const parsed = WebArgsSchema.safeParse(args ?? {});
    if (!parsed.success) {
      throw new InvalidInputError(parsed.error.issues.map((i) => i.message).join("; "));
    }
    const { count, start } = clampPaging(parsed.data.count, parsed.data.start);
    return { query: parsed.data.query, count, start, site: parsed.data.site };
  }

  parseImageArgs(args: unknown): { query: string; count: number; start: number } {
    const parsed = ImageArgsSchema.safeParse(args ?? {});
    if (!parsed.success) {
      throw new InvalidInputError(parsed.error.issues.map((i) => i.message).join("; "));
    }
    const { count, start } = clampPaging(parsed.data.count, parsed.data.start);
    return { query: parsed.data.query, count, start };
  }

  private buildUrl(params: {
    type: SearchType;
    query: string;
    count: number;
    start: number;
    site?: string;
  }): { url: URL; cacheKey: string } {
    let q = params.query;
    if (params.type === "web" && params.site && !params.query.includes("site:")) {
      q = `${params.query} site:${params.site}`;
    }
    const url = new URL("https://www.googleapis.com/customsearch/v1");
    url.searchParams.set("key", this.cfg.googleApiKey);
    url.searchParams.set("cx", this.cfg.googleCseId);
    url.searchParams.set("q", q);
    url.searchParams.set("num", String(params.count));
    url.searchParams.set("start", String(params.start));
    if (params.type === "image") url.searchParams.set("searchType", "image");

    // Cache key intentionally excludes the API key/secret.
    const cacheKey = JSON.stringify([params.type, q, params.count, params.start]);
    return { url, cacheKey };
  }

  async webSearch(
    args: { query: string; count: number; start: number; site: string },
    signal?: AbortSignal,
  ): Promise<string> {
    const { url, cacheKey } = this.buildUrl({ type: "web", ...args });
    return this.execute("web", url, cacheKey, signal);
  }

  async imageSearch(
    args: { query: string; count: number; start: number },
    signal?: AbortSignal,
  ): Promise<string> {
    const { url, cacheKey } = this.buildUrl({ type: "image", ...args });
    return this.execute("image", url, cacheKey, signal);
  }

  private async execute(
    type: SearchType,
    url: URL,
    cacheKey: string,
    signal?: AbortSignal,
  ): Promise<string> {
    const cached = this.cache.get(cacheKey);
    if (cached !== undefined) {
      this.logger.debug("cache hit", { type, cacheKey });
      return cached;
    }

    // Coalesce identical concurrent requests: share a single upstream call.
    const existing = this.inflight.get(cacheKey);
    if (existing) {
      this.logger.debug("coalescing in-flight request", { type, cacheKey });
      return existing;
    }

    const task = (async () => {
      await this.limiter.acquire(signal);
      const started = Date.now();
      if (type === "web") {
        const data = await this.http.getJson<GoogleResponse<GoogleItemWeb>>(url, signal);
        const text = this.formatWeb(data);
        this.cache.set(cacheKey, text);
        this.logger.info("web search complete", { ms: Date.now() - started, results: data.items?.length ?? 0 });
        return text;
      } else {
        const data = await this.http.getJson<GoogleResponse<GoogleItemImage>>(url, signal);
        const text = this.formatImage(data);
        this.cache.set(cacheKey, text);
        this.logger.info("image search complete", { ms: Date.now() - started, results: data.items?.length ?? 0 });
        return text;
      }
    })();

    this.inflight.set(cacheKey, task);
    try {
      return await task;
    } finally {
      this.inflight.delete(cacheKey);
    }
  }

  private assertNoApiError(data: GoogleResponse<unknown>): void {
    if (data.error) {
      throw new HttpApiError(data.error.code, data.error.message);
    }
  }

  private formatWeb(data: GoogleResponse<GoogleItemWeb>): string {
    this.assertNoApiError(data);
    if (!data.items || data.items.length === 0) return "No results found for your query.";
    const header = data.searchInformation?.totalResults
      ? `About ${data.searchInformation.totalResults} results.\n\n`
      : "";
    return (
      header +
      data.items
        .map((item, i) => {
          const title = item.title ?? "(no title)";
          const snippet = item.snippet ?? "No description";
          const link = item.link ?? "";
          return `[${i + 1}] Title: ${title}\nDescription: ${snippet}\nURL: ${link}`;
        })
        .join("\n\n")
    );
  }

  private formatImage(data: GoogleResponse<GoogleItemImage>): string {
    this.assertNoApiError(data);
    if (!data.items || data.items.length === 0) return "No image results found for your query.";
    return data.items
      .map((item, i) => {
        const title = item.title ?? "(no title)";
        const snippet = item.snippet || "No description";
        const link = item.link ?? "";
        const thumb = item.image?.thumbnailLink ? `\nThumbnail: ${item.image.thumbnailLink}` : "";
        return `[${i + 1}] Title: ${title}\nDescription: ${snippet}\nImage URL: ${link}${thumb}`;
      })
      .join("\n\n");
  }
}

/** Error returned inside a 200 OK Google response body. */
export class HttpApiError extends Error {
  constructor(
    readonly code: number,
    message: string,
  ) {
    super(`Google API error ${code}: ${message}`);
    this.name = "HttpApiError";
  }
}
