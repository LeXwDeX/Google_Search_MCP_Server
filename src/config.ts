/**
 * Validated, centralized configuration.
 *
 * Required secrets and all tunable knobs are parsed and bounds-checked once at
 * startup with zod. Invalid configuration fails fast with a clear message
 * instead of surfacing as confusing runtime errors later.
 */
import { z } from "zod";
import { LOG_LEVELS, type LogLevel } from "./logger.js";

/**
 * Coerce an environment string into an integer, applying a default when unset
 * or blank. Returns NaN for malformed input so zod can report it.
 */
const intFromEnv = (def: number) =>
  z
    .string()
    .trim()
    .optional()
    .transform((v) => (v === undefined || v === "" ? def : Number(v)))
    .pipe(z.number().int());

const ConfigSchema = z.object({
  // --- Required secrets ---
  googleApiKey: z.string().min(1, "GOOGLE_API_KEY environment variable is required"),
  googleCseId: z.string().min(1, "GOOGLE_CSE_ID environment variable is required"),

  // --- HTTP behaviour ---
  requestTimeoutMs: intFromEnv(10_000).pipe(z.number().int().min(1_000).max(120_000)),
  maxRetries: intFromEnv(3).pipe(z.number().int().min(0).max(8)),
  retryBaseDelayMs: intFromEnv(300).pipe(z.number().int().min(10).max(10_000)),
  retryMaxDelayMs: intFromEnv(8_000).pipe(z.number().int().min(100).max(60_000)),

  // --- Rate limiting / quota ---
  perSecondLimit: intFromEnv(5).pipe(z.number().int().min(1).max(100)),
  dailyLimit: intFromEnv(100).pipe(z.number().int().min(1).max(1_000_000)),
  rateLimitMaxWaitMs: intFromEnv(15_000).pipe(z.number().int().min(0).max(120_000)),

  // --- Cache ---
  cacheTtlMs: intFromEnv(300_000).pipe(z.number().int().min(0).max(86_400_000)),
  cacheMaxEntries: intFromEnv(500).pipe(z.number().int().min(0).max(100_000)),

  // --- Logging ---
  logLevel: z
    .string()
    .trim()
    .optional()
    .transform((v) => (v === undefined || v === "" ? "info" : v.toLowerCase()))
    .pipe(z.enum(LOG_LEVELS)),
});

export type Config = z.infer<typeof ConfigSchema> & { logLevel: LogLevel };

/** Hard limits imposed by the Google Custom Search JSON API. */
export const GOOGLE_LIMITS = {
  /** Max results per request. */
  maxCount: 10,
  /** start + num must not exceed 101; start itself maxes out at 91. */
  maxStart: 91,
  maxStartPlusNum: 101,
} as const;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const result = ConfigSchema.safeParse({
    googleApiKey: env.GOOGLE_API_KEY ?? "",
    googleCseId: env.GOOGLE_CSE_ID ?? "",
    requestTimeoutMs: env.GOOGLE_SEARCH_TIMEOUT_MS,
    maxRetries: env.GOOGLE_SEARCH_MAX_RETRIES,
    retryBaseDelayMs: env.GOOGLE_SEARCH_RETRY_BASE_MS,
    retryMaxDelayMs: env.GOOGLE_SEARCH_RETRY_MAX_MS,
    perSecondLimit: env.GOOGLE_SEARCH_PER_SECOND,
    dailyLimit: env.GOOGLE_SEARCH_DAILY_LIMIT,
    rateLimitMaxWaitMs: env.GOOGLE_SEARCH_RATE_WAIT_MS,
    cacheTtlMs: env.GOOGLE_SEARCH_CACHE_TTL_MS,
    cacheMaxEntries: env.GOOGLE_SEARCH_CACHE_MAX,
    logLevel: env.LOG_LEVEL,
  });

  if (!result.success) {
    const issues = result.error.issues
      .map((i) => `  - ${i.path.join(".") || "(root)"}: ${i.message}`)
      .join("\n");
    throw new Error(`Invalid configuration:\n${issues}`);
  }

  // Cross-field sanity: ensure max delay >= base delay.
  const cfg = result.data;
  if (cfg.retryMaxDelayMs < cfg.retryBaseDelayMs) {
    cfg.retryMaxDelayMs = cfg.retryBaseDelayMs;
  }
  return cfg as Config;
}
