//! Google Custom Search core.
//!
//! Single code path for both web and image search. Responsibilities:
//!  - Validate and clamp inputs to the API's hard limits.
//!  - Serve from cache when possible (and never burn quota on a cache hit).
//!  - Coalesce identical concurrent requests into one upstream call.
//!  - Enforce rate limits / quota before hitting the network.
//!  - Format results into a stable, human-readable text block.

use crate::cache::TtlLruCache;
use crate::cancel::{Cancel, ShutdownState};
use crate::config::{
    Config, GOOGLE_LIMITS_MAX_COUNT, GOOGLE_LIMITS_MAX_START, GOOGLE_LIMITS_MAX_START_PLUS_NUM,
};
use crate::error::AppError;
use crate::http::HttpClient;
use crate::logger::Logger;
use crate::rate_limiter::RateLimiter;
use crate::util::{is_safe_integer, js_trim, json_type_name, now_ms, UNDEFINED_TYPE};
use reqwest::Url;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, Notify};

const SEARCH_URL: &str = "https://www.googleapis.com/customsearch/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchType {
    Web,
    Image,
}

impl SearchType {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchType::Web => "web",
            SearchType::Image => "image",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebArgs {
    pub query: String,
    pub count: u32,
    pub start: u32,
    pub site: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArgs {
    pub query: String,
    pub count: u32,
    pub start: u32,
}

/// Clamp count/start into the API's valid ranges.
fn clamp_paging(count: f64, start: f64) -> (u32, u32) {
    let mut c = if count.is_finite() {
        count.trunc()
    } else {
        5.0
    };
    let mut s = if start.is_finite() {
        start.trunc()
    } else {
        1.0
    };
    c = c.clamp(1.0, GOOGLE_LIMITS_MAX_COUNT as f64);
    s = s.clamp(1.0, GOOGLE_LIMITS_MAX_START as f64);
    // start + num must not exceed 101.
    if s + c > GOOGLE_LIMITS_MAX_START_PLUS_NUM as f64 {
        c = (GOOGLE_LIMITS_MAX_START_PLUS_NUM as f64 - s).max(1.0);
    }
    (c as u32, s as u32)
}

/// JS `Number()` coercion used by zod's `z.coerce.number()`.
fn coerce_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => crate::util::js_number(s),
        Some(Value::Array(items)) => {
            if items.is_empty() {
                0.0
            } else if items.len() == 1 {
                coerce_number(Some(&items[0]))
            } else {
                f64::NAN
            }
        }
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `.int().catch(default)` semantics, including zod's safe-integer check.
fn coerce_int(value: Option<&Value>, default: i64) -> i64 {
    let n = coerce_number(value);
    if is_safe_integer(n) {
        n as i64
    } else {
        default
    }
}

fn expected(type_name: &str, received: &str) -> String {
    format!("Invalid input: expected {type_name}, received {received}")
}

fn received_type(value: Option<&Value>) -> &'static str {
    match value {
        Some(v) => json_type_name(v),
        None => UNDEFINED_TYPE,
    }
}

fn validate_query(args: &serde_json::Map<String, Value>, errors: &mut Vec<String>) -> String {
    match args.get("query") {
        Some(Value::String(raw)) => {
            let trimmed = js_trim(raw);
            if trimmed.is_empty() {
                errors.push("query must be a non-empty string".into());
            } else if trimmed.chars().count() > 2048 {
                errors.push("Too big: expected string to have <=2048 characters".into());
            }
            trimmed.to_string()
        }
        other => {
            errors.push(expected("string", received_type(other)));
            String::new()
        }
    }
}

fn validate_site(args: &serde_json::Map<String, Value>, errors: &mut Vec<String>) -> String {
    match args.get("site") {
        None => String::new(),
        Some(Value::String(raw)) => {
            let trimmed = js_trim(raw);
            if trimmed.chars().count() > 255 {
                errors.push("Too big: expected string to have <=255 characters".into());
            }
            trimmed.to_string()
        }
        other => {
            errors.push(expected("string", received_type(other)));
            String::new()
        }
    }
}

/// One coalesced in-flight search. Waiters wait on `notify`; the owner runs
/// the upstream call using only the shutdown token, so cancelling one
/// duplicate never fails the others.
struct SharedSlot {
    result: StdMutex<Option<Result<String, AppError>>>,
    notify: Notify,
}

impl SharedSlot {
    fn new() -> Self {
        Self {
            result: StdMutex::new(None),
            notify: Notify::new(),
        }
    }

    fn set(&self, value: Result<String, AppError>) {
        if let Ok(mut guard) = self.result.lock() {
            *guard = Some(value);
        }
        self.notify.notify_waiters();
    }

    fn get(&self) -> Option<Result<String, AppError>> {
        self.result.lock().ok().and_then(|guard| guard.clone())
    }
}

pub struct GoogleSearch {
    cfg: Config,
    http: HttpClient,
    limiter: Arc<RateLimiter>,
    cache: Arc<TtlLruCache>,
    logger: Logger,
    shutdown: ShutdownState,
    inflight: Mutex<InflightMap>,
}

type SharedResult = Arc<SharedSlot>;
type InflightMap = HashMap<String, SharedResult>;

impl GoogleSearch {
    pub fn new(
        cfg: Config,
        http: HttpClient,
        limiter: Arc<RateLimiter>,
        cache: Arc<TtlLruCache>,
        logger: Logger,
        shutdown: ShutdownState,
    ) -> Self {
        Self {
            cfg,
            http,
            limiter,
            cache,
            logger,
            shutdown,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Parse + validate raw tool arguments for the web search tool.
    pub fn parse_web_args(
        &self,
        args: Option<&serde_json::Map<String, Value>>,
    ) -> Result<WebArgs, AppError> {
        let empty = serde_json::Map::new();
        let args = args.unwrap_or(&empty);
        let mut errors = Vec::new();
        let query = validate_query(args, &mut errors);
        let count = coerce_int(args.get("count"), 5);
        let start = coerce_int(args.get("start"), 1);
        let site = validate_site(args, &mut errors);
        if !errors.is_empty() {
            return Err(AppError::InvalidInput(errors.join("; ")));
        }
        let (count, start) = clamp_paging(count as f64, start as f64);
        Ok(WebArgs {
            query,
            count,
            start,
            site,
        })
    }

    /// Parse + validate raw tool arguments for the image search tool.
    pub fn parse_image_args(
        &self,
        args: Option<&serde_json::Map<String, Value>>,
    ) -> Result<ImageArgs, AppError> {
        let empty = serde_json::Map::new();
        let args = args.unwrap_or(&empty);
        let mut errors = Vec::new();
        let query = validate_query(args, &mut errors);
        let count = coerce_int(args.get("count"), 5);
        let start = coerce_int(args.get("start"), 1);
        if !errors.is_empty() {
            return Err(AppError::InvalidInput(errors.join("; ")));
        }
        let (count, start) = clamp_paging(count as f64, start as f64);
        Ok(ImageArgs {
            query,
            count,
            start,
        })
    }

    fn build_url(
        &self,
        kind: SearchType,
        query: &str,
        count: u32,
        start: u32,
        site: &str,
    ) -> (Url, String) {
        let mut q = query.to_string();
        if kind == SearchType::Web && !site.is_empty() && !query.contains("site:") {
            q = format!("{query} site:{site}");
        }

        // JS URLSearchParams serialization (application/x-www-form-urlencoded):
        // spaces become `+`, everything else is percent-encoded.
        let mut pairs = vec![
            ("key", self.cfg.google_api_key.clone()),
            ("cx", self.cfg.google_cse_id.clone()),
            ("q", q.clone()),
            ("num", count.to_string()),
            ("start", start.to_string()),
        ];
        if kind == SearchType::Image {
            pairs.push(("searchType", "image".to_string()));
        }
        let query_string = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = Url::parse(&format!("{SEARCH_URL}?{query_string}"))
            .unwrap_or_else(|_| Url::parse(SEARCH_URL).expect("static URL"));

        // Cache key intentionally excludes the API key/secret.
        let cache_key =
            serde_json::to_string(&json!([kind.as_str(), q, count, start])).unwrap_or_default();
        (url, cache_key)
    }

    pub async fn web_search(&self, args: &WebArgs, cancel: Cancel) -> Result<String, AppError> {
        let (url, cache_key) = self.build_url(
            SearchType::Web,
            &args.query,
            args.count,
            args.start,
            &args.site,
        );
        self.execute(SearchType::Web, url, cache_key, cancel).await
    }

    pub async fn image_search(&self, args: &ImageArgs, cancel: Cancel) -> Result<String, AppError> {
        let (url, cache_key) =
            self.build_url(SearchType::Image, &args.query, args.count, args.start, "");
        self.execute(SearchType::Image, url, cache_key, cancel)
            .await
    }

    async fn execute(
        &self,
        kind: SearchType,
        url: Url,
        cache_key: String,
        cancel: Cancel,
    ) -> Result<String, AppError> {
        if let Some(cached) = self.cache.get(&cache_key) {
            self.logger.debug(
                "cache hit",
                json!({"type": kind.as_str(), "cacheKey": cache_key}),
            );
            return Ok(cached);
        }

        // Coalesce identical concurrent requests: share a single upstream call.
        let (slot, owner) = {
            let mut inflight = self.inflight.lock().await;
            match inflight.entry(cache_key.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => (entry.get().clone(), false),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    (entry.insert(Arc::new(SharedSlot::new())).clone(), true)
                }
            }
        };
        if !owner {
            self.logger.debug(
                "coalescing in-flight request",
                json!({"type": kind.as_str(), "cacheKey": cache_key}),
            );
            // Wait without ever driving the upstream call, so a waiter's
            // cancellation cannot abort work the owner still needs.
            loop {
                let notified = slot.notify.notified();
                if let Some(result) = slot.get() {
                    return result;
                }
                tokio::select! {
                    _ = notified => {}
                    _ = cancel.cancelled() => return Err(AppError::Aborted(cancel.reason())),
                }
            }
        }

        // Re-check after winning ownership: another request may have finished
        // between our first cache lookup and the in-flight map probe.
        if let Some(cached) = self.cache.get(&cache_key) {
            self.logger.debug(
                "cache hit",
                json!({"type": kind.as_str(), "cacheKey": cache_key}),
            );
            slot.set(Ok(cached.clone()));
            self.inflight.lock().await.remove(&cache_key);
            return Ok(cached);
        }

        // The shared upstream belongs to the connection, not to one request:
        // per-request cancellation only suppresses that request's response.
        let shutdown_cancel = Cancel::new(self.shutdown.clone());
        let result = self
            .run_upstream(kind, url, cache_key.clone(), shutdown_cancel)
            .await;
        slot.set(result.clone());
        self.inflight.lock().await.remove(&cache_key);
        result
    }

    async fn run_upstream(
        &self,
        kind: SearchType,
        url: Url,
        cache_key: String,
        cancel: Cancel,
    ) -> Result<String, AppError> {
        self.limiter.acquire(&cancel).await?;
        let started = now_ms();
        let data = self.http.get_json(&url, &cancel).await?;

        let (text, results) = match kind {
            SearchType::Web => {
                let text = format_web(&data)?;
                (text, item_count(&data))
            }
            SearchType::Image => {
                let text = format_image(&data)?;
                (text, item_count(&data))
            }
        };
        self.cache.set(&cache_key, text.clone());
        self.logger.info(
            &format!("{} search complete", kind.as_str()),
            json!({"ms": now_ms() - started, "results": results}),
        );
        Ok(text)
    }
}

fn item_count(data: &Value) -> usize {
    data.get("items")
        .and_then(|value| value.as_array())
        .map(Vec::len)
        .unwrap_or(0)
}

fn assert_no_api_error(data: &Value) -> Result<(), AppError> {
    let Some(error) = data.get("error") else {
        return Ok(());
    };
    if error.is_null() {
        return Ok(());
    }
    let code = error
        .get("code")
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite())
        .map(|value| value as i64)
        .unwrap_or(0);
    let message = error
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    Err(AppError::HttpApi {
        code,
        message: message.to_string(),
    })
}

fn format_web(data: &Value) -> Result<String, AppError> {
    assert_no_api_error(data)?;
    let Some(items) = data.get("items").and_then(|value| value.as_array()) else {
        return Ok("No results found for your query.".into());
    };
    if items.is_empty() {
        return Ok("No results found for your query.".into());
    }
    let header = data
        .get("searchInformation")
        .and_then(|value| value.get("totalResults"))
        .filter(|value| is_truthy(value))
        .map(|value| format!("About {} results.\n\n", js_string(value)))
        .unwrap_or_default();
    let body = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let title = field_default(item, "title", "(no title)");
            let snippet = field_default(item, "snippet", "No description");
            let link = field_or_empty(item, "link");
            format!(
                "[{}] Title: {}\nDescription: {}\nURL: {}",
                index + 1,
                title,
                snippet,
                link
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!("{header}{body}"))
}

fn format_image(data: &Value) -> Result<String, AppError> {
    assert_no_api_error(data)?;
    let Some(items) = data.get("items").and_then(|value| value.as_array()) else {
        return Ok("No image results found for your query.".into());
    };
    if items.is_empty() {
        return Ok("No image results found for your query.".into());
    }
    let body = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let title = field_default(item, "title", "(no title)");
            let snippet = field_truthy_default(item, "snippet", "No description");
            let link = field_or_empty(item, "link");
            let thumb = item
                .get("image")
                .and_then(|image| image.get("thumbnailLink"))
                .filter(|value| is_truthy(value))
                .map(|value| format!("\nThumbnail: {}", js_string(value)))
                .unwrap_or_default();
            format!(
                "[{}] Title: {}\nDescription: {}\nImage URL: {}{}",
                index + 1,
                title,
                snippet,
                link,
                thumb
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(body)
}

/// `value ?? fallback`: only null/undefined fall back.
fn field_default(item: &Value, field: &str, fallback: &str) -> String {
    match item.get(field) {
        Some(value) if !value.is_null() => js_string(value),
        _ => fallback.to_string(),
    }
}

/// `value || fallback`: every falsy value falls back.
fn field_truthy_default(item: &Value, field: &str, fallback: &str) -> String {
    match item.get(field) {
        Some(value) if is_truthy(value) => js_string(value),
        _ => fallback.to_string(),
    }
}

fn field_or_empty(item: &Value, field: &str) -> String {
    match item.get(field) {
        Some(value) if !value.is_null() => js_string(value),
        _ => String::new(),
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS template-literal stringification of a JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 1e21 {
                    format!("{}", f as i64)
                } else {
                    format!("{f}")
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `application/x-www-form-urlencoded` encoding used by `URLSearchParams`.
fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn google() -> GoogleSearch {
        let cfg = Config {
            google_api_key: "key".into(),
            google_cse_id: "cx".into(),
            request_timeout_ms: 1_000,
            max_retries: 0,
            retry_base_delay_ms: 10,
            retry_max_delay_ms: 100,
            per_second_limit: 5,
            daily_limit: 100,
            rate_limit_max_wait_ms: 0,
            cache_ttl_ms: 0,
            cache_max_entries: 0,
            log_level: crate::logger::LogLevel::Silent,
        };
        let logger = Logger::new(crate::logger::LogLevel::Silent);
        let http = HttpClient::new(
            crate::http::HttpOptions {
                timeout_ms: 1_000,
                max_retries: 0,
                base_delay_ms: 10,
                max_delay_ms: 100,
            },
            logger.clone(),
        )
        .unwrap();
        GoogleSearch::new(
            cfg,
            http,
            Arc::new(RateLimiter::new(5, 100, 0)),
            Arc::new(TtlLruCache::with_system_clock(0, 0)),
            logger,
            ShutdownState::new(),
        )
    }

    fn args(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn clamps_paging() {
        assert_eq!(clamp_paging(5.0, 1.0), (5, 1));
        assert_eq!(clamp_paging(999.0, 1.0), (10, 1));
        assert_eq!(clamp_paging(0.0, 0.0), (1, 1));
        assert_eq!(clamp_paging(10.0, 100.0), (10, 91));
    }

    #[test]
    fn query_errors_match_zod() {
        let search = google();
        let err = search
            .parse_web_args(Some(&args(json!({"query": ""}))))
            .unwrap_err();
        assert_eq!(err.raw_message(), "query must be a non-empty string");
        let err = search
            .parse_web_args(Some(&args(json!({"query": 123}))))
            .unwrap_err();
        assert_eq!(
            err.raw_message(),
            "Invalid input: expected string, received number"
        );
        let err = search.parse_web_args(None).unwrap_err();
        assert_eq!(
            err.raw_message(),
            "Invalid input: expected string, received undefined"
        );
        let err = search
            .parse_web_args(Some(&args(json!({"query": "a".repeat(2049)}))))
            .unwrap_err();
        assert_eq!(
            err.raw_message(),
            "Too big: expected string to have <=2048 characters"
        );
    }

    #[test]
    fn coerce_count_behaviour() {
        let search = google();
        assert_eq!(
            search
                .parse_web_args(Some(&args(json!({"query":"x","count":3.7}))))
                .unwrap()
                .count,
            5
        );
        assert_eq!(
            search
                .parse_web_args(Some(&args(json!({"query":"x","count":"abc"}))))
                .unwrap()
                .count,
            5
        );
        assert_eq!(
            search
                .parse_web_args(Some(&args(json!({"query":"x","count":"3"}))))
                .unwrap()
                .count,
            3
        );
        assert_eq!(
            search
                .parse_web_args(Some(&args(json!({"query":"x","count":null}))))
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            search
                .parse_web_args(Some(&args(json!({"query":"x","start":100000}))))
                .unwrap()
                .start,
            91
        );
    }

    #[test]
    fn unsafe_integers_fall_back_to_defaults() {
        let search = google();
        for value in [
            json!(1e21),
            json!(9_007_199_254_740_992u64),
            json!(-9_007_199_254_740_992i64),
        ] {
            let parsed = search
                .parse_web_args(Some(&args(json!({"query": "x", "count": value}))))
                .unwrap();
            assert_eq!(parsed.count, 5);
        }
    }

    #[test]
    fn length_limits_count_code_points() {
        let search = google();
        let emoji = "😀";
        assert!(search
            .parse_web_args(Some(&args(json!({"query": emoji.repeat(1025)}))))
            .is_ok());
        let err = search
            .parse_web_args(Some(&args(json!({"query": emoji.repeat(2049)}))))
            .unwrap_err();
        assert_eq!(
            err.raw_message(),
            "Too big: expected string to have <=2048 characters"
        );
        assert!(search
            .parse_web_args(Some(&args(json!({"query":"x","site":emoji.repeat(255)}))))
            .is_ok());
    }

    #[test]
    fn site_is_trimmed_and_applied() {
        let search = google();
        let parsed = search
            .parse_web_args(Some(&args(json!({"query":"x","site":"  example.com  "}))))
            .unwrap();
        assert_eq!(parsed.site, "example.com");
        let (url, _) = search.build_url(SearchType::Web, "x", 5, 1, &parsed.site);
        let query = url.query().unwrap();
        assert!(query.contains("q=x+site%3Aexample.com"));
    }

    #[test]
    fn formats_web_and_image() {
        let data = json!({
            "searchInformation": {"totalResults": "42"},
            "items": [{"title": "T", "link": "L", "snippet": "S"}]
        });
        assert_eq!(
            format_web(&data).unwrap(),
            "About 42 results.\n\n[1] Title: T\nDescription: S\nURL: L"
        );
        assert_eq!(
            format_web(&json!({})).unwrap(),
            "No results found for your query."
        );
        let images = json!({
            "items": [{"title": "T", "link": "L", "snippet": "", "image": {"thumbnailLink": "TH"}}]
        });
        assert_eq!(
            format_image(&images).unwrap(),
            "[1] Title: T\nDescription: No description\nImage URL: L\nThumbnail: TH"
        );
    }

    #[test]
    fn api_error_is_typed() {
        let err = format_web(&json!({"error": {"code": 400, "message": "bad"}})).unwrap_err();
        assert_eq!(err.user_message(), "Google API error 400: bad");
    }

    #[test]
    fn form_encode_matches_url_search_params() {
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("q=1&x"), "q%3D1%26x");
        assert_eq!(form_encode("中文"), "%E4%B8%AD%E6%96%87");
    }
}
