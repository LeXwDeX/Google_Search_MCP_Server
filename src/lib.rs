//! Google Search MCP server core.
//!
//! The binary speaks MCP over stdio: newline-delimited JSON-RPC 2.0 on
//! stdin/stdout, with every log line on stderr. It serves the 2026-07-28
//! stateless protocol revision (per-request `_meta` envelope) and answers
//! 2025-era `initialize` handshakes from the same connection.

#![forbid(unsafe_code)]

pub mod cache;
pub mod cancel;
pub mod config;
pub mod dotenv;
pub mod error;
pub mod google;
pub mod http;
pub mod logger;
pub mod protocol;
pub mod rate_limiter;
pub mod util;
