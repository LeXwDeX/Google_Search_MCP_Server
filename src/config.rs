//! Validated, centralized configuration.
//!
//! Required secrets and every tunable knob are parsed and bounds-checked once
//! at startup. Invalid configuration fails fast with the same messages the
//! TypeScript build emitted, so operators see a stable contract.

use crate::logger::LogLevel;
use crate::util::{js_number, js_trim, MAX_SAFE_INTEGER};

/// Fully validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub google_api_key: String,
    pub google_cse_id: String,
    pub request_timeout_ms: u64,
    pub max_retries: u32,
    pub retry_base_delay_ms: u64,
    pub retry_max_delay_ms: u64,
    pub per_second_limit: u32,
    pub daily_limit: u32,
    pub rate_limit_max_wait_ms: u64,
    pub cache_ttl_ms: u64,
    pub cache_max_entries: usize,
    pub log_level: LogLevel,
}

/// Hard limits imposed by the Google Custom Search JSON API.
pub const GOOGLE_LIMITS_MAX_COUNT: u64 = 10;
pub const GOOGLE_LIMITS_MAX_START: u64 = 91;
pub const GOOGLE_LIMITS_MAX_START_PLUS_NUM: u64 = 101;

struct Issue {
    field: &'static str,
    message: String,
}

/// Load and validate configuration from the process environment.
///
/// Returns the fatal startup message on any issue, matching the TypeScript
/// server's stderr output.
pub fn load_config() -> Result<Config, String> {
    load_config_from(|name| std::env::var(name).ok())
}

/// Injectable variant for tests.
pub fn load_config_from(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let mut issues: Vec<Issue> = Vec::new();

    let google_api_key = match get("GOOGLE_API_KEY") {
        Some(v) if !v.is_empty() => v,
        _ => {
            issues.push(Issue {
                field: "googleApiKey",
                message: "GOOGLE_API_KEY environment variable is required".into(),
            });
            String::new()
        }
    };
    let google_cse_id = match get("GOOGLE_CSE_ID") {
        Some(v) if !v.is_empty() => v,
        _ => {
            issues.push(Issue {
                field: "googleCseId",
                message: "GOOGLE_CSE_ID environment variable is required".into(),
            });
            String::new()
        }
    };

    let mut int = |field: &'static str, name: &str, default: u64, min: u64, max: u64| {
        let raw = get(name);
        let trimmed = raw.as_deref().map(js_trim).unwrap_or("");
        let value = if trimmed.is_empty() {
            default as f64
        } else {
            js_number(trimmed)
        };
        if value.is_nan() {
            issues.push(Issue {
                field,
                message: "Invalid input: expected number, received NaN".into(),
            });
            return default;
        }
        if value.is_infinite() {
            issues.push(Issue {
                field,
                message: "Invalid input: expected number, received number".into(),
            });
            return default;
        }
        if value.fract() != 0.0 {
            issues.push(Issue {
                field,
                message: "Invalid input: expected int, received number".into(),
            });
            return default;
        }
        if value > MAX_SAFE_INTEGER {
            issues.push(Issue {
                field,
                message: "Too big: expected int to be <=9007199254740991".into(),
            });
            return default;
        }
        if value < -MAX_SAFE_INTEGER {
            issues.push(Issue {
                field,
                message: "Too small: expected int to be >=-9007199254740991".into(),
            });
            return default;
        }
        if value < min as f64 {
            issues.push(Issue {
                field,
                message: format!("Too small: expected number to be >={min}"),
            });
            return default;
        }
        if value > max as f64 {
            issues.push(Issue {
                field,
                message: format!("Too big: expected number to be <={max}"),
            });
            return default;
        }
        value as u64
    };

    let request_timeout_ms = int(
        "requestTimeoutMs",
        "GOOGLE_SEARCH_TIMEOUT_MS",
        10_000,
        1_000,
        120_000,
    );
    let max_retries = int("maxRetries", "GOOGLE_SEARCH_MAX_RETRIES", 3, 0, 8) as u32;
    let retry_base_delay_ms = int(
        "retryBaseDelayMs",
        "GOOGLE_SEARCH_RETRY_BASE_MS",
        300,
        10,
        10_000,
    );
    let retry_max_delay_ms = int(
        "retryMaxDelayMs",
        "GOOGLE_SEARCH_RETRY_MAX_MS",
        8_000,
        100,
        60_000,
    );
    let per_second_limit = int("perSecondLimit", "GOOGLE_SEARCH_PER_SECOND", 5, 1, 100) as u32;
    let daily_limit = int("dailyLimit", "GOOGLE_SEARCH_DAILY_LIMIT", 100, 1, 1_000_000) as u32;
    let rate_limit_max_wait_ms = int(
        "rateLimitMaxWaitMs",
        "GOOGLE_SEARCH_RATE_WAIT_MS",
        15_000,
        0,
        120_000,
    );
    let cache_ttl_ms = int(
        "cacheTtlMs",
        "GOOGLE_SEARCH_CACHE_TTL_MS",
        300_000,
        0,
        86_400_000,
    );
    let cache_max_entries = int(
        "cacheMaxEntries",
        "GOOGLE_SEARCH_CACHE_MAX",
        500,
        0,
        100_000,
    ) as usize;

    let log_level = {
        let raw = get("LOG_LEVEL");
        let trimmed = raw.as_deref().map(js_trim).unwrap_or("");
        let value = if trimmed.is_empty() {
            "info".to_string()
        } else {
            trimmed.to_ascii_lowercase()
        };
        match LogLevel::parse(&value) {
            Some(level) => level,
            None => {
                issues.push(Issue {
                    field: "logLevel",
                    message:
                        "Invalid option: expected one of \"debug\"|\"info\"|\"warn\"|\"error\"|\"silent\""
                            .into(),
                });
                LogLevel::Info
            }
        }
    };

    if !issues.is_empty() {
        let mut message = String::from("Invalid configuration:");
        for issue in issues {
            message.push_str(&format!("\n  - {}: {}", issue.field, issue.message));
        }
        return Err(message);
    }

    // Cross-field sanity: max delay must not be below base delay.
    let retry_max_delay_ms = retry_max_delay_ms.max(retry_base_delay_ms);

    Ok(Config {
        google_api_key,
        google_cse_id,
        request_timeout_ms,
        max_retries,
        retry_base_delay_ms,
        retry_max_delay_ms,
        per_second_limit,
        daily_limit,
        rate_limit_max_wait_ms,
        cache_ttl_ms,
        cache_max_entries,
        log_level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        load_config_from(|name| map.get(name).cloned())
    }

    #[test]
    fn missing_secrets() {
        let err = load(&[]).unwrap_err();
        assert_eq!(
            err,
            "Invalid configuration:\n  - googleApiKey: GOOGLE_API_KEY environment variable is required\n  - googleCseId: GOOGLE_CSE_ID environment variable is required"
        );
    }

    #[test]
    fn invalid_ints_match_zod_messages() {
        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "abc"),
            ("GOOGLE_SEARCH_MAX_RETRIES", "9"),
            ("LOG_LEVEL", "nope"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid configuration:\n  - requestTimeoutMs: Invalid input: expected number, received NaN\n  - maxRetries: Too big: expected number to be <=8\n  - logLevel: Invalid option: expected one of \"debug\"|\"info\"|\"warn\"|\"error\"|\"silent\""
        );
    }

    #[test]
    fn float_is_rejected_as_int() {
        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "1000.5"),
        ])
        .unwrap_err();
        assert!(err.contains("Invalid input: expected int, received number"));
    }

    #[test]
    fn blank_optional_uses_default() {
        let cfg = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "  "),
        ])
        .unwrap();
        assert_eq!(cfg.request_timeout_ms, 10_000);
    }

    #[test]
    fn max_delay_is_raised_to_base() {
        let cfg = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_RETRY_BASE_MS", "5000"),
            ("GOOGLE_SEARCH_RETRY_MAX_MS", "1000"),
        ])
        .unwrap();
        assert_eq!(cfg.retry_max_delay_ms, 5000);
    }

    #[test]
    fn infinity_and_safe_integer_messages() {
        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "Infinity"),
        ])
        .unwrap_err();
        assert!(err.contains("requestTimeoutMs: Invalid input: expected number, received number"));

        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_MAX_RETRIES", "9223372036854775807"),
        ])
        .unwrap_err();
        assert!(err.contains("maxRetries: Too big: expected int to be <=9007199254740991"));

        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_MAX_RETRIES", "-9223372036854775807"),
        ])
        .unwrap_err();
        assert!(err.contains("maxRetries: Too small: expected int to be >=-9007199254740991"));
    }

    #[test]
    fn js_whitespace_is_used_for_env_values() {
        let cfg = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "\u{FEFF}1000"),
            ("LOG_LEVEL", "\u{FEFF}debug"),
        ])
        .unwrap();
        assert_eq!(cfg.request_timeout_ms, 1000);
        assert_eq!(cfg.log_level, LogLevel::Debug);

        let err = load(&[
            ("GOOGLE_API_KEY", "k"),
            ("GOOGLE_CSE_ID", "c"),
            ("GOOGLE_SEARCH_TIMEOUT_MS", "\u{0085}1000"),
        ])
        .unwrap_err();
        assert!(err.contains("Invalid input: expected number, received NaN"));
    }
}
