#!/usr/bin/env node
/**
 * Google Search MCP server — entry point.
 *
 * Wires together config, logging, cache, rate limiting, the resilient HTTP
 * client and the search core, exposes two MCP tools, and installs robust
 * lifecycle handling (graceful shutdown, crash guards).
 */
import dotenv from "dotenv";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  type Tool,
} from "@modelcontextprotocol/sdk/types.js";

import { loadConfig } from "./config.js";
import { createLogger } from "./logger.js";
import { TtlLruCache } from "./cache.js";
import { RateLimiter, QuotaExceededError, RateLimitTimeoutError } from "./rateLimiter.js";
import { HttpClient, HttpError } from "./http.js";
import { GoogleSearch, InvalidInputError, HttpApiError } from "./google.js";

dotenv.config();

const WEB_SEARCH_TOOL: Tool = {
  name: "google_web_search",
  description:
    "Performs a web search using the Google Custom Search API, ideal for general queries, news, articles, and online content. " +
    "Use this for broad information gathering, recent events, or when you need diverse web sources. " +
    "Supports pagination and filtering by site or type. " +
    "Maximum 10 results per request, with start index for pagination. ",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string", description: "Search query" },
      count: { type: "number", description: "Number of results (1-10, default 5)", default: 5 },
      start: { type: "number", description: "Pagination start index (1-91, default 1)", default: 1 },
      site: {
        type: "string",
        description: "Optional: Limit search to specific site (e.g., 'example.com')",
        default: "",
      },
    },
    required: ["query"],
  },
};

const IMAGE_SEARCH_TOOL: Tool = {
  name: "google_image_search",
  description:
    "Searches for images using Google's Custom Search API. " +
    "Best for finding images related to specific terms, concepts, or objects. " +
    "Returns image URLs, titles, and thumbnails. " +
    "Use this when needing to find relevant images or visual references.",
  inputSchema: {
    type: "object",
    properties: {
      query: { type: "string", description: "Image search query" },
      count: { type: "number", description: "Number of results (1-10, default 5)", default: 5 },
      start: { type: "number", description: "Pagination start index (1-91, default 1)", default: 1 },
    },
    required: ["query"],
  },
};

/** Map any thrown error to a stable, user-facing message (no secrets). */
function toUserMessage(error: unknown): string {
  if (error instanceof InvalidInputError) return `Invalid input: ${error.message}`;
  if (error instanceof QuotaExceededError) return error.message;
  if (error instanceof RateLimitTimeoutError) return error.message;
  if (error instanceof HttpApiError) return error.message;
  if (error instanceof HttpError) {
    const detail = error.body ? ` — ${error.body.slice(0, 500)}` : "";
    return `${error.message}${detail}`;
  }
  if (error instanceof Error) {
    if (error.name === "AbortError" || /aborted|timed out/i.test(error.message)) {
      return `Request failed: ${error.message}`;
    }
    return `Error: ${error.message}`;
  }
  return `Error: ${String(error)}`;
}

async function main(): Promise<void> {
  let config;
  try {
    config = loadConfig();
  } catch (err) {
    // Config errors are fatal and printed to stderr before any logger exists.
    process.stderr.write(`${err instanceof Error ? err.message : String(err)}\n`);
    process.exit(1);
    return;
  }

  const logger = createLogger(config.logLevel);
  const cache = new TtlLruCache<string>(config.cacheMaxEntries, config.cacheTtlMs);
  const limiter = new RateLimiter(config.perSecondLimit, config.dailyLimit, config.rateLimitMaxWaitMs);
  const http = new HttpClient({
    timeoutMs: config.requestTimeoutMs,
    maxRetries: config.maxRetries,
    baseDelayMs: config.retryBaseDelayMs,
    maxDelayMs: config.retryMaxDelayMs,
    logger: logger.child({ component: "http" }),
  });
  const search = new GoogleSearch(config, http, limiter, cache, logger.child({ component: "search" }));

  // Cancels in-flight upstream requests when the process is shutting down.
  const shutdownController = new AbortController();

  // Periodically prune expired cache entries to bound memory.
  const pruneTimer = setInterval(() => {
    const removed = cache.prune();
    if (removed > 0) logger.debug("cache pruned", { removed, ...cache.stats() });
  }, 60_000);
  pruneTimer.unref?.();

  const server = new Server(
    { name: "example-servers/google-search", version: "0.2.0" },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [WEB_SEARCH_TOOL, IMAGE_SEARCH_TOOL],
  }));

  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const { name, arguments: args } = request.params;
    try {
      switch (name) {
        case "google_web_search": {
          const parsed = search.parseWebArgs(args);
          const text = await search.webSearch(parsed, shutdownController.signal);
          return { content: [{ type: "text", text }], isError: false };
        }
        case "google_image_search": {
          const parsed = search.parseImageArgs(args);
          const text = await search.imageSearch(parsed, shutdownController.signal);
          return { content: [{ type: "text", text }], isError: false };
        }
        default:
          return { content: [{ type: "text", text: `Unknown tool: ${name}` }], isError: true };
      }
    } catch (error) {
      logger.error("tool call failed", {
        tool: name,
        error: error instanceof Error ? error.message : String(error),
      });
      return { content: [{ type: "text", text: toUserMessage(error) }], isError: true };
    }
  });

  const transport = new StdioServerTransport();
  await server.connect(transport);
  logger.info("Google Search MCP Server running on stdio", {
    perSecondLimit: config.perSecondLimit,
    dailyLimit: config.dailyLimit,
    cacheTtlMs: config.cacheTtlMs,
  });

  // --- Lifecycle: graceful shutdown & crash guards ---
  let closing = false;
  const shutdown = async (reason: string, code: number) => {
    if (closing) return;
    closing = true;
    logger.info("shutting down", { reason, cache: cache.stats(), rate: limiter.stats() });
    clearInterval(pruneTimer);
    shutdownController.abort(new Error(`server shutting down: ${reason}`));
    try {
      await server.close();
    } catch (err) {
      logger.error("error during shutdown", { error: err instanceof Error ? err.message : String(err) });
    }
    process.exit(code);
  };

  process.on("SIGINT", () => void shutdown("SIGINT", 0));
  process.on("SIGTERM", () => void shutdown("SIGTERM", 0));
  // stdio transport closing (client disconnect) should end the process.
  transport.onclose = () => void shutdown("transport closed", 0);

  process.on("uncaughtException", (err) => {
    logger.error("uncaughtException", { error: err.message, stack: err.stack });
    void shutdown("uncaughtException", 1);
  });
  process.on("unhandledRejection", (reason) => {
    logger.error("unhandledRejection", { reason: reason instanceof Error ? reason.message : String(reason) });
  });
}

main().catch((error) => {
  process.stderr.write(`Fatal error running server: ${error instanceof Error ? error.stack ?? error.message : String(error)}\n`);
  process.exit(1);
});
