//! Google Search MCP server — entry point.
//!
//! Wires together config, logging, cache, rate limiting, the resilient HTTP
//! client and the search core, then runs the MCP v2 stdio server (2026-07-28
//! stateless revision, with the 2025-era initialize handshake as fallback).
//!
//! This is a native CLI binary: the protocol is spoken directly over
//! stdin/stdout. There is no Node runtime and no in-process SDK to boot.

use google_search_mcp::cache::TtlLruCache;
use google_search_mcp::cancel::ShutdownState;
use google_search_mcp::config::load_config;
use google_search_mcp::google::GoogleSearch;
use google_search_mcp::http::{HttpClient, HttpOptions};
use google_search_mcp::logger::Logger;
use google_search_mcp::protocol::{self, ServerDeps};
use google_search_mcp::rate_limiter::RateLimiter;
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Match `dotenv.config()`: load `.env` from the working directory only,
    // never overriding variables already present in the environment.
    google_search_mcp::dotenv::load_and_apply();

    let config = match load_config() {
        Ok(config) => config,
        Err(message) => {
            // Config errors are fatal and printed before any logger exists.
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

    let logger = Logger::new(config.log_level);

    let panic_logger = logger.clone();
    std::panic::set_hook(Box::new(move |info| {
        panic_logger.error(
            "uncaughtException",
            json!({
                "error": info.to_string(),
                "stack": info.location().map(|location| format!("{location}")).unwrap_or_default(),
            }),
        );
        std::process::exit(1);
    }));

    let cache = Arc::new(TtlLruCache::with_system_clock(
        config.cache_max_entries,
        config.cache_ttl_ms,
    ));
    let limiter = Arc::new(RateLimiter::new(
        config.per_second_limit,
        config.daily_limit,
        config.rate_limit_max_wait_ms,
    ));

    let http = match HttpClient::new(
        HttpOptions {
            timeout_ms: config.request_timeout_ms,
            max_retries: config.max_retries,
            base_delay_ms: config.retry_base_delay_ms,
            max_delay_ms: config.retry_max_delay_ms,
        },
        logger.child(json!({"component": "http"})),
    ) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("Fatal error running server: {err}");
            std::process::exit(1);
        }
    };

    let shutdown = ShutdownState::new();

    let search = Arc::new(GoogleSearch::new(
        config.clone(),
        http,
        limiter.clone(),
        cache.clone(),
        logger.child(json!({"component": "search"})),
        shutdown.clone(),
    ));

    let deps = ServerDeps {
        search,
        limiter,
        cache,
        cache_ttl_ms: config.cache_ttl_ms,
        logger,
    };

    let code = protocol::run(deps, shutdown).await;
    std::process::exit(code);
}
