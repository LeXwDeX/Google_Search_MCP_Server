//! Resilient HTTP client for the Google API.
//!
//! Adds the guarantees the native fetch lacks for production use:
//!  - Per-attempt timeout (no unbounded hangs).
//!  - Retries on transient failures (network errors, 408/425/429/5xx).
//!  - Exponential backoff with full jitter, capped at a max delay.
//!  - Honors the server's `Retry-After` header when present.
//!  - Respects an external cancellation signal (graceful shutdown or
//!    `notifications/cancelled`).

use crate::cancel::Cancel;
use crate::error::AppError;
use crate::logger::Logger;
use crate::util::{js_number, now_ms};
use reqwest::Url;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const RETRYABLE_STATUS: &[u16] = &[408, 425, 429, 500, 502, 503, 504];

#[derive(Debug, Clone, Copy)]
pub struct HttpOptions {
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

pub struct HttpClient {
    client: reqwest::Client,
    opts: HttpOptions,
    logger: Logger,
}

impl HttpClient {
    pub fn new(opts: HttpOptions, logger: Logger) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()?;
        Ok(Self {
            client,
            opts,
            logger,
        })
    }

    fn backoff(&self, attempt: u32, retry_after_ms: Option<i64>) -> i64 {
        if let Some(retry_after) = retry_after_ms {
            return retry_after.min(self.opts.max_delay_ms as i64);
        }
        let exp = self.opts.base_delay_ms as f64 * 2f64.powi(attempt as i32);
        let capped = exp.min(self.opts.max_delay_ms as f64);
        // Full jitter: random in [0, capped].
        (rand::random::<f64>() * capped).floor() as i64
    }

    /// GET a URL and return the parsed JSON body.
    pub async fn get_json(&self, url: &Url, cancel: &Cancel) -> Result<Value, AppError> {
        let mut last_err: Option<AppError> = None;

        for attempt in 0..=self.opts.max_retries {
            if cancel.is_cancelled() {
                return Err(AppError::Aborted(cancel.reason()));
            }

            let request = async {
                let response = self
                    .client
                    .get(url.clone())
                    .header(reqwest::header::ACCEPT, "application/json")
                    .header(reqwest::header::USER_AGENT, "google-search-mcp/0.2")
                    .send()
                    .await?;
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                if status.is_success() {
                    let body = response.text().await?;
                    Ok::<_, reqwest::Error>((status, retry_after, Some(body), None))
                } else {
                    // The TS build swallows body-read failures on error
                    // responses: the status still wins, with an empty body.
                    let body = response.text().await.unwrap_or_default();
                    Ok((status, retry_after, None, Some(body)))
                }
            };

            let outcome = tokio::select! {
                result = tokio::time::timeout(
                    Duration::from_millis(self.opts.timeout_ms),
                    request,
                ) => result,
                _ = cancel.cancelled() => return Err(AppError::Aborted(cancel.reason())),
            };

            let result = match outcome {
                Err(_) => Err(AppError::HttpTimeout(format!(
                    "request timed out after {}ms",
                    self.opts.timeout_ms
                ))),
                Ok(Err(_)) => Err(AppError::HttpNetwork("fetch failed".into())),
                Ok(Ok((status, retry_after, success_body, error_body))) => {
                    if status.is_success() {
                        let body = success_body.unwrap_or_default();
                        serde_json::from_str::<Value>(&body)
                            .map_err(|err| AppError::HttpJson(node_json_error(&body, &err)))
                    } else {
                        let body = error_body.unwrap_or_default();
                        let message = format!("Google API HTTP {} ", status.as_u16());
                        let retryable = RETRYABLE_STATUS.contains(&status.as_u16());
                        if !retryable || attempt == self.opts.max_retries {
                            // Non-retryable (or out of attempts): fail now
                            // instead of entering the network-retry path.
                            return Err(AppError::HttpStatus { message, body });
                        } else {
                            let retry_after_ms = retry_after
                                .as_deref()
                                .and_then(|header| parse_retry_after(header, now_ms()));
                            let delay = self.backoff(attempt, retry_after_ms);
                            self.logger.warn(
                                "retrying after retryable HTTP status",
                                json!({
                                    "status": status.as_u16(),
                                    "attempt": attempt + 1,
                                    "maxRetries": self.opts.max_retries,
                                    "delayMs": delay,
                                }),
                            );
                            last_err = Some(AppError::HttpStatus { message, body });
                            sleep_or_cancel(delay, cancel).await?;
                            continue;
                        }
                    }
                }
            };

            match result {
                Ok(value) => return Ok(value),
                Err(err) => {
                    // External cancellation: do not retry.
                    if cancel.is_cancelled() {
                        return Err(AppError::Aborted(cancel.reason()));
                    }
                    if attempt == self.opts.max_retries {
                        return Err(err);
                    }
                    let delay = self.backoff(attempt, None);
                    self.logger.warn(
                        "retrying after network/timeout error",
                        json!({
                            "attempt": attempt + 1,
                            "maxRetries": self.opts.max_retries,
                            "delayMs": delay,
                            "error": err.raw_message(),
                        }),
                    );
                    last_err = Some(err);
                    sleep_or_cancel(delay, cancel).await?;
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| AppError::HttpNetwork("request failed after retries".into())))
    }
}

async fn sleep_or_cancel(ms: i64, cancel: &Cancel) -> Result<(), AppError> {
    if ms <= 0 {
        return Ok(());
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms as u64)) => Ok(()),
        _ = cancel.cancelled() => Err(AppError::Aborted(cancel.reason())),
    }
}

/// Parse Retry-After (delta seconds, ISO-8601, or HTTP date) into milliseconds.
fn parse_retry_after(header: &str, now: i64) -> Option<i64> {
    // TS: `if (!header) return undefined` — only the empty string bails;
    // whitespace is Number(" ") === 0.
    if header.is_empty() {
        return None;
    }
    let as_number = js_number(header);
    if as_number.is_finite() {
        return Some((as_number * 1000.0).max(0.0) as i64);
    }
    if let Ok(date) = OffsetDateTime::parse(header, &Rfc3339) {
        let millis = date.unix_timestamp_nanos() / 1_000_000;
        return Some(((millis as i64) - now).max(0));
    }
    let date = httpdate::parse_http_date(header).ok()?;
    let millis = date
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((millis - now).max(0))
}

/// Approximate the V8 `JSON.parse` message used by Node's `response.json()`.
fn node_json_error(body: &str, err: &serde_json::Error) -> String {
    let trimmed = body.trim_start();
    if trimmed.is_empty() {
        return "Unexpected end of JSON input".into();
    }
    if err.to_string().contains("trailing characters") {
        let column = err.column();
        return format!(
            "Unexpected non-whitespace character after JSON at position {} (line {} column {})",
            column.saturating_sub(1),
            err.line(),
            column
        );
    }
    let first = trimmed.chars().next().unwrap_or('?');
    format!("Unexpected token '{first}', \"{body}\" is not valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_and_date() {
        assert_eq!(parse_retry_after("2", 0), Some(2_000));
        assert_eq!(parse_retry_after("0.5", 0), Some(500));
        assert_eq!(parse_retry_after("abc", 0), None);
        assert_eq!(parse_retry_after("", 0), None);
        assert_eq!(parse_retry_after(" ", 0), Some(0));
        assert_eq!(
            parse_retry_after("1970-01-01T00:00:03Z", 1_000),
            Some(2_000)
        );
        // Thu, 01 Jan 1970 00:00:03 GMT
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:03 GMT", 1_000),
            Some(2_000)
        );
    }

    #[test]
    fn json_errors_look_like_v8() {
        let err = serde_json::from_str::<Value>("<html>oops</html>").unwrap_err();
        assert_eq!(
            node_json_error("<html>oops</html>", &err),
            "Unexpected token '<', \"<html>oops</html>\" is not valid JSON"
        );
        let err = serde_json::from_str::<Value>("").unwrap_err();
        assert_eq!(node_json_error("", &err), "Unexpected end of JSON input");
        let err = serde_json::from_str::<Value>("{\"a\":1}x").unwrap_err();
        assert_eq!(
            node_json_error("{\"a\":1}x", &err),
            "Unexpected non-whitespace character after JSON at position 7 (line 1 column 8)"
        );
    }

    #[test]
    fn backoff_respects_retry_after_and_cap() {
        let opts = HttpOptions {
            timeout_ms: 1_000,
            max_retries: 3,
            base_delay_ms: 100,
            max_delay_ms: 250,
        };
        let client = HttpClient::new(opts, Logger::new(crate::logger::LogLevel::Silent)).unwrap();
        assert_eq!(client.backoff(0, Some(10_000)), 250);
        assert_eq!(client.backoff(0, Some(120)), 120);
        for _ in 0..20 {
            let delay = client.backoff(0, None);
            assert!((0..=100).contains(&delay));
        }
    }
}
