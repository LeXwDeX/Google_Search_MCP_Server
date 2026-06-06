/**
 * Leveled, structured logger.
 *
 * CRITICAL: On an MCP stdio transport, stdout is reserved for the JSON-RPC
 * protocol stream. Writing anything else to stdout corrupts the protocol.
 * Therefore every log line goes to stderr.
 */

export const LOG_LEVELS = ["debug", "info", "warn", "error", "silent"] as const;
export type LogLevel = (typeof LOG_LEVELS)[number];

const LEVEL_WEIGHT: Record<LogLevel, number> = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40,
  silent: 100,
};

export function isLogLevel(value: unknown): value is LogLevel {
  return typeof value === "string" && (LOG_LEVELS as readonly string[]).includes(value);
}

export interface Logger {
  level: LogLevel;
  debug(message: string, fields?: Record<string, unknown>): void;
  info(message: string, fields?: Record<string, unknown>): void;
  warn(message: string, fields?: Record<string, unknown>): void;
  error(message: string, fields?: Record<string, unknown>): void;
  child(bindings: Record<string, unknown>): Logger;
}

/** Keys whose values must never be logged in full. */
const SENSITIVE_KEYS = new Set(["key", "apiKey", "api_key", "token", "authorization", "password"]);

function redact(fields: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(fields)) {
    out[k] = SENSITIVE_KEYS.has(k) ? "[redacted]" : v;
  }
  return out;
}

function format(level: LogLevel, message: string, fields: Record<string, unknown>): string {
  const base: Record<string, unknown> = {
    ts: new Date().toISOString(),
    level,
    msg: message,
    ...redact(fields),
  };
  try {
    return JSON.stringify(base);
  } catch {
    // Fall back to a safe representation if a field is not serializable.
    return JSON.stringify({ ts: base.ts, level, msg: message, fieldsError: "unserializable" });
  }
}

function make(level: LogLevel, bindings: Record<string, unknown>): Logger {
  const threshold = LEVEL_WEIGHT[level];

  const write = (lvl: LogLevel, message: string, fields?: Record<string, unknown>) => {
    if (LEVEL_WEIGHT[lvl] < threshold) return;
    process.stderr.write(format(lvl, message, { ...bindings, ...(fields ?? {}) }) + "\n");
  };

  return {
    level,
    debug: (m, f) => write("debug", m, f),
    info: (m, f) => write("info", m, f),
    warn: (m, f) => write("warn", m, f),
    error: (m, f) => write("error", m, f),
    child: (extra) => make(level, { ...bindings, ...extra }),
  };
}

export function createLogger(level: LogLevel = "info"): Logger {
  return make(level, {});
}
