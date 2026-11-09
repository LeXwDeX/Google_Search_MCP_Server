//! Errors surfaced to MCP clients and to the structured log.

use crate::util::utf16_slice;

/// Every failure path the search core can produce.
///
/// Cloning is cheap enough for request coalescing, and lets one upstream
/// result be shared by every waiter without re-running the request.
#[derive(Debug, Clone)]
pub enum AppError {
    /// Tool arguments failed validation.
    InvalidInput(String),
    /// The local daily quota is exhausted.
    QuotaExceeded {
        message: String,
        used: u32,
        limit: u32,
        reset_at: i64,
    },
    /// Waiting for a per-second token would exceed the configured ceiling.
    RateLimitTimeout(String),
    /// Google answered 200 OK but embedded an error object.
    HttpApi { code: i64, message: String },
    /// A non-2xx HTTP status.
    HttpStatus { message: String, body: String },
    /// The per-attempt timeout elapsed.
    HttpTimeout(String),
    /// A network-level failure (connect, TLS, body read).
    HttpNetwork(String),
    /// The response body was not valid JSON.
    HttpJson(String),
    /// Shutdown or client cancellation.
    Aborted(String),
    /// Anything else.
    Other(String),
}

impl AppError {
    /// Message as the TypeScript server logs it (`error.message`).
    pub fn raw_message(&self) -> String {
        match self {
            AppError::InvalidInput(m)
            | AppError::RateLimitTimeout(m)
            | AppError::HttpTimeout(m)
            | AppError::HttpNetwork(m)
            | AppError::HttpJson(m)
            | AppError::Aborted(m)
            | AppError::Other(m) => m.clone(),
            AppError::QuotaExceeded { message, .. } => message.clone(),
            AppError::HttpApi { code, message } => format!("Google API error {code}: {message}"),
            AppError::HttpStatus { message, .. } => message.clone(),
        }
    }

    /// Stable, user-facing text returned in the tool result.
    pub fn user_message(&self) -> String {
        match self {
            AppError::InvalidInput(m) => format!("Invalid input: {m}"),
            AppError::QuotaExceeded { message, .. } => message.clone(),
            AppError::RateLimitTimeout(m) => m.clone(),
            AppError::HttpApi { code, message } => format!("Google API error {code}: {message}"),
            AppError::HttpStatus { message, body } => {
                if body.is_empty() {
                    message.clone()
                } else {
                    format!("{message} — {}", utf16_slice(body, 500))
                }
            }
            AppError::HttpTimeout(m) => format!("Request failed: {m}"),
            AppError::HttpNetwork(m)
            | AppError::HttpJson(m)
            | AppError::Aborted(m)
            | AppError::Other(m) => {
                if is_abort_or_timeout(m) {
                    format!("Request failed: {m}")
                } else {
                    format!("Error: {m}")
                }
            }
        }
    }
}

fn is_abort_or_timeout(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("aborted") || lower.contains("timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_http_body_at_500_utf16_units() {
        let err = AppError::HttpStatus {
            message: "Google API HTTP 400 ".into(),
            body: "x".repeat(600),
        };
        let msg = err.user_message();
        assert_eq!(msg, format!("Google API HTTP 400  — {}", "x".repeat(500)));
    }

    #[test]
    fn timeout_reads_as_request_failed() {
        let err = AppError::HttpTimeout("request timed out after 1000ms".into());
        assert_eq!(
            err.user_message(),
            "Request failed: request timed out after 1000ms"
        );
    }

    #[test]
    fn plain_network_error_keeps_error_prefix() {
        let err = AppError::HttpNetwork("fetch failed".into());
        assert_eq!(err.user_message(), "Error: fetch failed");
    }
}
