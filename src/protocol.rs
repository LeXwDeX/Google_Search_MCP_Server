//! MCP v2 wire server over stdio.
//!
//! Reads newline-delimited JSON-RPC 2.0 from stdin and writes responses to
//! stdout. Serves the 2026-07-28 stateless revision (per-request `_meta`
//! envelope, `resultType`, cache hints) and falls back to the 2025-era
//! `initialize` handshake for older clients on the same connection — the same
//! era selection and skill surface as the TypeScript build.
//!
//! This is plain CLI communication: no embedded runtime, no SDK process.

use crate::cache::TtlLruCache;
use crate::cancel::{Cancel, ShutdownState};
use crate::google::{GoogleSearch, ImageArgs, WebArgs};
use crate::logger::Logger;
use crate::rate_limiter::RateLimiter;
use crate::util::{json_type_name, UNDEFINED_TYPE};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const SERVER_NAME: &str = "example-servers/google-search";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const PROTOCOL_VERSION: &str = "2026-07-28";
pub const SUPPORTED_MODERN_VERSIONS: &[&str] = &[PROTOCOL_VERSION];

const PV_META: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_INFO_META: &str = "io.modelcontextprotocol/clientInfo";
const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO_META: &str = "io.modelcontextprotocol/serverInfo";
const SUBSCRIPTION_ID_META: &str = "io.modelcontextprotocol/subscriptionId";
const LOG_LEVEL_META: &str = "io.modelcontextprotocol/logLevel";
const RELATED_TASK_META: &str = "io.modelcontextprotocol/related-task";

const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const METHOD_NOT_FOUND: i64 = -32601;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const STDIO_MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;
const MAX_SUBSCRIPTIONS: usize = 1024;
const TOOLS_LIST_TTL_MS: u64 = 300_000;

#[derive(Clone)]
pub struct ServerDeps {
    pub search: Arc<GoogleSearch>,
    pub limiter: Arc<RateLimiter>,
    pub cache: Arc<TtlLruCache>,
    pub cache_ttl_ms: u64,
    pub logger: Logger,
}

#[derive(Debug, Clone)]
struct ShutdownRequest {
    reason: String,
    code: i32,
}

enum Phase {
    Opening,
    Probe,
    Modern,
    Legacy,
}

struct Incoming {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

impl Incoming {
    fn is_request(&self) -> bool {
        self.id.is_some()
    }

    fn params_map(&self) -> Option<&Map<String, Value>> {
        self.params.as_ref().and_then(|value| value.as_object())
    }

    fn meta(&self) -> Option<&Map<String, Value>> {
        self.params_map()
            .and_then(|params| params.get("_meta"))
            .and_then(|meta| meta.as_object())
    }

    fn has_envelope_claim(&self) -> bool {
        self.meta()
            .map(|meta| meta.contains_key(PV_META))
            .unwrap_or(false)
    }

    fn claimed_version(&self) -> Option<&str> {
        self.meta()
            .and_then(|meta| meta.get(PV_META))
            .and_then(|value| value.as_str())
    }

    /// Strict JSON-RPC request/notification shape (mirrors the SDK's
    /// `.strict()` schemas: extra top-level keys, array params, null ids and
    /// non-integer numeric ids are rejected).
    fn parse(value: Value) -> Option<Self> {
        let object = value.as_object()?;
        for key in object.keys() {
            match key.as_str() {
                "jsonrpc" | "id" | "method" | "params" => {}
                _ => return None,
            }
        }
        if object.get("jsonrpc")?.as_str()? != "2.0" {
            return None;
        }
        let method = object.get("method")?.as_str()?.to_string();
        if let Some(params) = object.get("params") {
            let params = params.as_object()?;
            if !is_valid_transport_meta(params) {
                return None;
            }
        }
        let id = match object.get("id") {
            None => None,
            Some(id) => {
                if !is_valid_request_id(id) {
                    return None;
                }
                Some(normalize_id(id))
            }
        };
        Some(Self {
            id,
            method,
            params: object.get("params").cloned(),
        })
    }
}

fn is_valid_request_id(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        // RequestIdSchema is z.number().int(): finite, integral, and within the
        // IEEE-754 safe-integer range. JS JSON.parse rounds larger literals.
        Value::Number(number) => number
            .as_f64()
            .map(crate::util::is_safe_integer)
            .unwrap_or(false),
        _ => false,
    }
}

/// The transport-level `RequestMetaSchema` (loose): `_meta` must be an object,
/// `progressToken` (if present) a string or safe integer, and the related-task
/// metadata (if present) an object with a string `taskId`. Messages that fail
/// are dropped by the JSON-RPC schema layer, not answered.
fn is_valid_transport_meta(params: &Map<String, Value>) -> bool {
    match params.get("_meta") {
        None => true,
        Some(Value::Object(meta)) => {
            if let Some(token) = meta.get("progressToken") {
                if !is_progress_token(token) {
                    return false;
                }
            }
            if let Some(task) = meta.get(RELATED_TASK_META) {
                let Some(task) = task.as_object() else {
                    return false;
                };
                if !task.get("taskId").map(Value::is_string).unwrap_or(false) {
                    return false;
                }
            }
            true
        }
        Some(_) => false,
    }
}
fn normalize_id(id: &Value) -> Value {
    if let Value::Number(number) = id {
        if let Some(f) = number.as_f64() {
            if crate::util::is_safe_integer(f) {
                return json!(f as i64);
            }
        }
    }
    id.clone()
}

fn is_modern_version(version: &str) -> bool {
    version >= PROTOCOL_VERSION
}

/// Legacy `initialize` versions the SDK accepts verbatim; anything else
/// negotiates down to the latest legacy revision.
const LEGACY_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

fn negotiate_legacy_version(requested: &str) -> &str {
    if LEGACY_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        "2025-11-25"
    }
}

fn missing_envelope_message() -> String {
    format!(
        "Request is missing the required _meta envelope for protocol revision {PROTOCOL_VERSION} ({PV_META}, {CLIENT_CAPABILITIES_META})"
    )
}

/// One zod-shaped validation issue. `path` is relative to the outermost
/// object being validated (the envelope itself or initialize params).
#[derive(Debug, Clone)]
struct SchemaIssue {
    expected: String,
    path: Vec<String>,
    message: String,
}

impl SchemaIssue {
    /// Opening-phase envelope errors name the offending key path.
    fn key(&self) -> String {
        if self.path.is_empty() {
            "_meta".to_string()
        } else {
            self.path.join(".")
        }
    }
}

fn type_name_of(value: Option<&Value>) -> &'static str {
    match value {
        Some(v) => json_type_name(v),
        None => UNDEFINED_TYPE,
    }
}

fn push_type_issue(
    issues: &mut Vec<SchemaIssue>,
    expected: &str,
    path: Vec<String>,
    value: Option<&Value>,
) {
    issues.push(SchemaIssue {
        expected: expected.to_string(),
        path,
        message: format!(
            "Invalid input: expected {expected}, received {}",
            type_name_of(value)
        ),
    });
}

fn join_path(base: &[String], extra: &[&str]) -> Vec<String> {
    let mut out = base.to_vec();
    out.extend(extra.iter().map(|part| (*part).to_string()));
    out
}

fn validate_string_field(
    object: &Map<String, Value>,
    field: &str,
    required: bool,
    base: &[String],
    issues: &mut Vec<SchemaIssue>,
) {
    match object.get(field) {
        Some(Value::String(_)) => {}
        Some(other) => push_type_issue(issues, "string", join_path(base, &[field]), Some(other)),
        None if required => push_type_issue(issues, "string", join_path(base, &[field]), None),
        None => {}
    }
}

/// `ImplementationSchema`: name, title, icons, version, websiteUrl, description.
fn validate_implementation(value: &Value, base: &[String], issues: &mut Vec<SchemaIssue>) {
    let Some(info) = value.as_object() else {
        push_type_issue(issues, "object", base.to_vec(), Some(value));
        return;
    };
    validate_string_field(info, "name", true, base, issues);
    validate_string_field(info, "title", false, base, issues);

    if let Some(icons) = info.get("icons") {
        match icons {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let mut icon_path = join_path(base, &["icons"]);
                    icon_path.push(index.to_string());
                    let Some(icon) = item.as_object() else {
                        push_type_issue(issues, "object", icon_path, Some(item));
                        continue;
                    };
                    match icon.get("src") {
                        Some(Value::String(_)) => {}
                        Some(other) => push_type_issue(
                            issues,
                            "string",
                            join_path(&icon_path, &["src"]),
                            Some(other),
                        ),
                        None => {
                            push_type_issue(issues, "string", join_path(&icon_path, &["src"]), None)
                        }
                    }
                    validate_string_field(icon, "mimeType", false, &icon_path, issues);
                    if let Some(sizes) = icon.get("sizes") {
                        match sizes {
                            Value::Array(values) => {
                                for (size_index, size) in values.iter().enumerate() {
                                    if !size.is_string() {
                                        let mut size_path = join_path(&icon_path, &["sizes"]);
                                        size_path.push(size_index.to_string());
                                        push_type_issue(issues, "string", size_path, Some(size));
                                    }
                                }
                            }
                            other => push_type_issue(
                                issues,
                                "array",
                                join_path(&icon_path, &["sizes"]),
                                Some(other),
                            ),
                        }
                    }
                    if let Some(theme) = icon.get("theme") {
                        if !matches!(theme.as_str(), Some("light") | Some("dark")) {
                            issues.push(SchemaIssue {
                                expected: "enum".into(),
                                path: join_path(&icon_path, &["theme"]),
                                message: "Invalid option: expected one of \"light\"|\"dark\""
                                    .into(),
                            });
                        }
                    }
                }
            }
            other => push_type_issue(issues, "array", join_path(base, &["icons"]), Some(other)),
        }
    }

    validate_string_field(info, "version", true, base, issues);
    validate_string_field(info, "websiteUrl", false, base, issues);
    validate_string_field(info, "description", false, base, issues);
}

/// `ClientCapabilitiesSchema` (legacy, with tasks) and the 2026 variant
/// (without tasks). JSONObject members are records whose values must be
/// objects.
fn validate_client_capabilities(
    value: &Value,
    base: &[String],
    include_tasks: bool,
    issues: &mut Vec<SchemaIssue>,
) {
    let Some(caps) = value.as_object() else {
        push_type_issue(issues, "object", base.to_vec(), Some(value));
        return;
    };

    if let Some(experimental) = caps.get("experimental") {
        match experimental.as_object() {
            Some(record) => {
                for (key, member) in record {
                    if !member.is_object() {
                        push_type_issue(
                            issues,
                            "record",
                            join_path(base, &["experimental", key]),
                            Some(member),
                        );
                    }
                }
            }
            None => push_type_issue(
                issues,
                "record",
                join_path(base, &["experimental"]),
                Some(experimental),
            ),
        }
    }

    if let Some(sampling) = caps.get("sampling") {
        match sampling.as_object() {
            Some(record) => {
                for field in ["context", "tools"] {
                    if let Some(member) = record.get(field) {
                        if !member.is_object() {
                            push_type_issue(
                                issues,
                                "record",
                                join_path(base, &["sampling", field]),
                                Some(member),
                            );
                        }
                    }
                }
            }
            None => push_type_issue(
                issues,
                "object",
                join_path(base, &["sampling"]),
                Some(sampling),
            ),
        }
    }

    if let Some(elicitation) = caps.get("elicitation") {
        match elicitation.as_object() {
            Some(record) => {
                if let Some(form) = record.get("form") {
                    match form.as_object() {
                        Some(form) => {
                            if let Some(apply_defaults) = form.get("applyDefaults") {
                                if !apply_defaults.is_boolean() {
                                    push_type_issue(
                                        issues,
                                        "boolean",
                                        join_path(base, &["elicitation", "form", "applyDefaults"]),
                                        Some(apply_defaults),
                                    );
                                }
                            }
                        }
                        None => push_type_issue(
                            issues,
                            "object",
                            join_path(base, &["elicitation", "form"]),
                            Some(form),
                        ),
                    }
                }
                if let Some(url) = record.get("url") {
                    if !url.is_object() {
                        push_type_issue(
                            issues,
                            "record",
                            join_path(base, &["elicitation", "url"]),
                            Some(url),
                        );
                    }
                }
            }
            None => push_type_issue(
                issues,
                "object",
                join_path(base, &["elicitation"]),
                Some(elicitation),
            ),
        }
    }

    if let Some(roots) = caps.get("roots") {
        match roots.as_object() {
            Some(record) => {
                if let Some(list_changed) = record.get("listChanged") {
                    if !list_changed.is_boolean() {
                        push_type_issue(
                            issues,
                            "boolean",
                            join_path(base, &["roots", "listChanged"]),
                            Some(list_changed),
                        );
                    }
                }
            }
            None => push_type_issue(issues, "object", join_path(base, &["roots"]), Some(roots)),
        }
    }

    if include_tasks {
        if let Some(tasks) = caps.get("tasks") {
            match tasks.as_object() {
                Some(record) => {
                    for field in ["list", "cancel"] {
                        if let Some(member) = record.get(field) {
                            if !member.is_object() {
                                push_type_issue(
                                    issues,
                                    "record",
                                    join_path(base, &["tasks", field]),
                                    Some(member),
                                );
                            }
                        }
                    }
                    if let Some(requests) = record.get("requests") {
                        match requests.as_object() {
                            Some(requests) => {
                                for field in ["sampling", "elicitation"] {
                                    if let Some(member) = requests.get(field) {
                                        if !member.is_object() {
                                            push_type_issue(
                                                issues,
                                                "object",
                                                join_path(base, &["tasks", "requests", field]),
                                                Some(member),
                                            );
                                        }
                                    }
                                }
                            }
                            None => push_type_issue(
                                issues,
                                "object",
                                join_path(base, &["tasks", "requests"]),
                                Some(requests),
                            ),
                        }
                    }
                }
                None => push_type_issue(issues, "object", join_path(base, &["tasks"]), Some(tasks)),
            }
        }
    }

    if let Some(extensions) = caps.get("extensions") {
        match extensions.as_object() {
            Some(record) => {
                for (key, member) in record {
                    if !member.is_object() {
                        push_type_issue(
                            issues,
                            "record",
                            join_path(base, &["extensions", key]),
                            Some(member),
                        );
                    }
                }
            }
            None => push_type_issue(
                issues,
                "record",
                join_path(base, &["extensions"]),
                Some(extensions),
            ),
        }
    }
}

/// The 2026-07-28 `RequestMetaEnvelopeSchema`.
fn validate_envelope_schema(
    meta: &Map<String, Value>,
    include_tasks: bool,
    issues: &mut Vec<SchemaIssue>,
) {
    if let Some(token) = meta.get("progressToken") {
        if !is_progress_token(token) {
            issues.push(SchemaIssue {
                expected: "string or integer".into(),
                path: vec!["progressToken".into()],
                message: format!(
                    "Invalid input: expected string, received {}",
                    json_type_name(token)
                ),
            });
        }
    }

    match meta.get(PV_META) {
        Some(Value::String(_)) => {}
        Some(other) => push_type_issue(issues, "string", vec![PV_META.into()], Some(other)),
        None => push_type_issue(issues, "string", vec![PV_META.into()], None),
    }

    if let Some(info) = meta.get(CLIENT_INFO_META) {
        validate_implementation(info, &[CLIENT_INFO_META.to_string()], issues);
    }

    match meta.get(CLIENT_CAPABILITIES_META) {
        Some(value) => validate_client_capabilities(
            value,
            &[CLIENT_CAPABILITIES_META.to_string()],
            include_tasks,
            issues,
        ),
        None => push_type_issue(
            issues,
            "object",
            vec![CLIENT_CAPABILITIES_META.into()],
            None,
        ),
    }

    if let Some(level) = meta.get(LOG_LEVEL_META) {
        match level.as_str() {
            Some(value) if is_logging_level(value) => {}
            Some(_) => issues.push(SchemaIssue {
                expected: "enum".into(),
                path: vec![LOG_LEVEL_META.into()],
                message: "Invalid option: expected one of \"debug\"|\"info\"|\"notice\"|\"warning\"|\"error\"|\"critical\"|\"alert\"|\"emergency\"".into(),
            }),
            None => push_type_issue(issues, "string", vec![LOG_LEVEL_META.into()], Some(level)),
        }
    }
}

/// Opening-phase envelope validation: required-key presence first (`missing`),
/// then the schema's own issues.
fn validate_envelope_opening(meta: &Map<String, Value>) -> Vec<SchemaIssue> {
    let mut issues = Vec::new();
    for key in [PV_META, CLIENT_CAPABILITIES_META] {
        if !meta.contains_key(key) {
            issues.push(SchemaIssue {
                expected: String::new(),
                path: vec![key.to_string()],
                message: "missing".into(),
            });
        }
    }
    let mut schema = Vec::new();
    validate_envelope_schema(meta, false, &mut schema);
    for issue in schema {
        if issue.path.len() == 1
            && issues
                .iter()
                .any(|existing| existing.path == issue.path && existing.message == "missing")
        {
            continue;
        }
        issues.push(issue);
    }
    issues
}

/// Pinned-modern envelope validation: no required-key precheck, no version
/// check; the schema's issue messages are joined by the caller.
fn validate_envelope_pinned(meta: &Map<String, Value>) -> Vec<SchemaIssue> {
    let mut issues = Vec::new();
    validate_envelope_schema(meta, false, &mut issues);
    issues
}

fn is_progress_token(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Number(number) => number
            .as_f64()
            .map(crate::util::is_safe_integer)
            .unwrap_or(false),
        _ => false,
    }
}

fn is_logging_level(value: &str) -> bool {
    matches!(
        value,
        "debug" | "info" | "notice" | "warning" | "error" | "critical" | "alert" | "emergency"
    )
}

/// The SDK's `initialize` precedence rule: only a well-formed modern claim
/// overrides the legacy handshake classification.
fn carries_valid_modern_claim(message: &Incoming) -> bool {
    let Some(meta) = message.meta() else {
        return false;
    };
    if !meta.contains_key(PV_META) {
        return false;
    }
    let Some(version) = message.claimed_version() else {
        return false;
    };
    if !is_modern_version(version) {
        return false;
    }
    validate_envelope_opening(meta).is_empty()
}

enum Classification {
    Legacy,
    InvalidEnvelope(SchemaIssue),
    Unsupported(String),
    Modern,
}

fn classify(message: &Incoming) -> Classification {
    if message.method == "initialize" && !carries_valid_modern_claim(message) {
        return Classification::Legacy;
    }
    if !message.has_envelope_claim() {
        return Classification::Legacy;
    }
    let Some(meta) = message.meta() else {
        return Classification::Legacy;
    };
    if let Some(issue) = validate_envelope_opening(meta).into_iter().next() {
        return Classification::InvalidEnvelope(issue);
    }
    match message.claimed_version() {
        Some(version) if SUPPORTED_MODERN_VERSIONS.contains(&version) => Classification::Modern,
        Some(other) => Classification::Unsupported(other.to_string()),
        None => Classification::Unsupported("unknown".to_string()),
    }
}
pub fn tools_list() -> Value {
    json!([
        {
            "name": "google_web_search",
            "description": "Performs a web search using the Google Custom Search API, ideal for general queries, news, articles, and online content. Use this for broad information gathering, recent events, or when you need diverse web sources. Supports pagination and filtering by site or type. Maximum 10 results per request, with start index for pagination. ",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "count": { "type": "number", "description": "Number of results (1-10, default 5)", "default": 5 },
                    "start": { "type": "number", "description": "Pagination start index (1-91, default 1)", "default": 1 },
                    "site": { "type": "string", "description": "Optional: Limit search to specific site (e.g., 'example.com')", "default": "" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "google_image_search",
            "description": "Searches for images using Google's Custom Search API. Best for finding images related to specific terms, concepts, or objects. Returns image URLs, titles, and thumbnails. Use this when needing to find relevant images or visual references.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Image search query" },
                    "count": { "type": "number", "description": "Number of results (1-10, default 5)", "default": 5 },
                    "start": { "type": "number", "description": "Pagination start index (1-91, default 1)", "default": 1 }
                },
                "required": ["query"]
            }
        }
    ])
}

fn server_info() -> Value {
    json!({"name": SERVER_NAME, "version": SERVER_VERSION})
}

fn server_info_meta() -> Value {
    let mut meta = Map::new();
    meta.insert(SERVER_INFO_META.into(), server_info());
    Value::Object(meta)
}

struct ZodIssue {
    expected: String,
    path: Vec<String>,
    message: String,
}

impl ZodIssue {
    fn new(expected: &str, path: &[&str], message: String) -> Self {
        Self {
            expected: expected.to_string(),
            path: path.iter().map(|part| part.to_string()).collect(),
            message,
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("expected".into(), Value::String(self.expected.clone()));
        map.insert("code".into(), Value::String("invalid_type".into()));
        map.insert(
            "path".into(),
            Value::Array(self.path.iter().cloned().map(Value::String).collect()),
        );
        map.insert("message".into(), Value::String(self.message.clone()));
        Value::Object(map)
    }
}

fn format_issues(issues: &[ZodIssue]) -> String {
    serde_json::to_string_pretty(&Value::Array(
        issues.iter().map(ZodIssue::to_value).collect(),
    ))
    .unwrap_or_default()
}

fn expected_type(expected: &str, received: &str) -> String {
    format!("Invalid input: expected {expected}, received {received}")
}

fn to_zod_issues(issues: Vec<SchemaIssue>) -> Vec<ZodIssue> {
    issues
        .into_iter()
        .map(|issue| ZodIssue {
            expected: issue.expected,
            path: issue.path,
            message: issue.message,
        })
        .collect()
}

/// Validate `initialize` params; returns the requested protocol version.
fn validate_initialize(params: Option<&Value>) -> Result<String, Vec<ZodIssue>> {
    let mut schema = Vec::new();
    let params = match params {
        Some(Value::Object(params)) => params,
        Some(other) => {
            push_type_issue(&mut schema, "object", vec!["params".into()], Some(other));
            return Err(to_zod_issues(schema));
        }
        None => {
            push_type_issue(&mut schema, "object", vec!["params".into()], None);
            return Err(to_zod_issues(schema));
        }
    };

    let mut requested = String::new();
    match params.get("protocolVersion") {
        Some(Value::String(version)) => requested = version.clone(),
        Some(other) => push_type_issue(
            &mut schema,
            "string",
            vec!["params".into(), "protocolVersion".into()],
            Some(other),
        ),
        None => push_type_issue(
            &mut schema,
            "string",
            vec!["params".into(), "protocolVersion".into()],
            None,
        ),
    }

    let capabilities_base = vec!["params".to_string(), "capabilities".to_string()];
    match params.get("capabilities") {
        Some(value) => validate_client_capabilities(value, &capabilities_base, true, &mut schema),
        None => push_type_issue(&mut schema, "object", capabilities_base, None),
    }

    let client_info_base = vec!["params".to_string(), "clientInfo".to_string()];
    match params.get("clientInfo") {
        Some(value) => validate_implementation(value, &client_info_base, &mut schema),
        None => push_type_issue(&mut schema, "object", client_info_base, None),
    }

    if schema.is_empty() {
        Ok(requested)
    } else {
        Err(to_zod_issues(schema))
    }
}

fn validate_cursor(params: Option<&Value>) -> Result<(), Vec<ZodIssue>> {
    let Some(Value::Object(params)) = params else {
        return Ok(());
    };
    match params.get("cursor") {
        None => Ok(()),
        Some(Value::String(_)) => Ok(()),
        Some(other) => Err(vec![ZodIssue::new(
            "string",
            &["params", "cursor"],
            expected_type("string", json_type_name(other)),
        )]),
    }
}

#[derive(Debug)]
struct ToolCallParams {
    name: String,
    arguments: Option<Map<String, Value>>,
}

enum SearchTask {
    Web(WebArgs),
    Image(ImageArgs),
}

impl SearchTask {
    fn tool_name(&self) -> &'static str {
        match self {
            SearchTask::Web(_) => "google_web_search",
            SearchTask::Image(_) => "google_image_search",
        }
    }
}

fn tool_result_value(text: String, is_error: bool, modern: bool) -> Value {
    let mut content = Map::new();
    content.insert("type".into(), Value::String("text".into()));
    content.insert("text".into(), Value::String(text));
    let mut result = Map::new();
    result.insert("content".into(), Value::Array(vec![Value::Object(content)]));
    result.insert("isError".into(), Value::Bool(is_error));
    if modern {
        result.insert("resultType".into(), Value::String("complete".into()));
        result.insert("_meta".into(), server_info_meta());
    }
    Value::Object(result)
}

fn validate_tools_call(
    params: Option<&Value>,
    modern: bool,
) -> Result<ToolCallParams, Vec<ZodIssue>> {
    let mut issues = Vec::new();
    let params = match params {
        Some(Value::Object(params)) => params,
        Some(other) => {
            issues.push(ZodIssue::new(
                "object",
                &["params"],
                expected_type("object", json_type_name(other)),
            ));
            return Err(issues);
        }
        None => {
            issues.push(ZodIssue::new(
                "object",
                &["params"],
                expected_type("object", UNDEFINED_TYPE),
            ));
            return Err(issues);
        }
    };

    // Legacy CallToolRequestParamsSchema extends TaskAugmentedRequestParamsSchema;
    // the 2026 wire schema has no task field.
    if !modern {
        if let Some(task) = params.get("task") {
            match task {
                Value::Object(task) => {
                    if let Some(ttl) = task.get("ttl") {
                        if !ttl.is_number() {
                            issues.push(ZodIssue::new(
                                "number",
                                &["params", "task", "ttl"],
                                expected_type("number", json_type_name(ttl)),
                            ));
                        }
                    }
                }
                other => issues.push(ZodIssue::new(
                    "object",
                    &["params", "task"],
                    expected_type("object", json_type_name(other)),
                )),
            }
        }
    }

    let mut name = String::new();
    match params.get("name") {
        Some(Value::String(value)) => name = value.clone(),
        Some(other) => issues.push(ZodIssue::new(
            "string",
            &["params", "name"],
            expected_type("string", json_type_name(other)),
        )),
        None => issues.push(ZodIssue::new(
            "string",
            &["params", "name"],
            expected_type("string", UNDEFINED_TYPE),
        )),
    }

    let mut arguments = None;
    match params.get("arguments") {
        None => {}
        Some(Value::Null) => issues.push(ZodIssue::new(
            "record",
            &["params", "arguments"],
            expected_type("record", "null"),
        )),
        Some(Value::Object(map)) => arguments = Some(map.clone()),
        Some(other) => issues.push(ZodIssue::new(
            "record",
            &["params", "arguments"],
            expected_type("record", json_type_name(other)),
        )),
    }

    if issues.is_empty() {
        Ok(ToolCallParams { name, arguments })
    } else {
        Err(issues)
    }
}

/// `notifications/cancelled` cancels in-flight work only for truthy request ids;
/// the SDK's cancellation handler bails on `0` and `""`.
fn is_truthy_request_id(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `requestState` must be a string when present (both eras).
fn invalid_request_state(params: Option<&Value>) -> bool {
    params
        .and_then(|params| params.as_object())
        .and_then(|params| params.get("requestState"))
        .map(|value| !value.is_string())
        .unwrap_or(false)
}

struct Connection {
    phase: Phase,
    subscriptions: IndexMap<String, Value>,
    inflight: Arc<Mutex<HashMap<String, CancellationToken>>>,
    writer: mpsc::UnboundedSender<String>,
    deps: ServerDeps,
    shutdown: ShutdownState,
}

impl Connection {
    fn new(
        deps: ServerDeps,
        writer: mpsc::UnboundedSender<String>,
        shutdown: ShutdownState,
    ) -> Self {
        Self {
            phase: Phase::Opening,
            subscriptions: IndexMap::new(),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            writer,
            deps,
            shutdown,
        }
    }

    fn send(&self, value: Value) {
        if let Ok(line) = serde_json::to_string(&value) {
            let _ = self.writer.send(line);
        }
    }

    fn send_result(&self, id: &Value, result: Value) {
        let mut message = Map::new();
        message.insert("result".into(), result);
        message.insert("jsonrpc".into(), Value::String("2.0".into()));
        message.insert("id".into(), id.clone());
        self.send(Value::Object(message));
    }

    fn send_error(&self, id: &Value, code: i64, message: String, data: Option<Value>) {
        let mut error = Map::new();
        error.insert("code".into(), json!(code));
        error.insert("message".into(), Value::String(message));
        if let Some(data) = data {
            error.insert("data".into(), data);
        }
        let mut response = Map::new();
        response.insert("jsonrpc".into(), Value::String("2.0".into()));
        response.insert("id".into(), id.clone());
        response.insert("error".into(), Value::Object(error));
        self.send(Value::Object(response));
    }

    fn send_notification(&self, method: &str, params: Value) {
        let mut message = Map::new();
        message.insert("jsonrpc".into(), Value::String("2.0".into()));
        message.insert("method".into(), Value::String(method.into()));
        message.insert("params".into(), params);
        self.send(Value::Object(message));
    }

    async fn handle(&mut self, message: Incoming) {
        match self.phase {
            Phase::Opening => self.handle_opening(message).await,
            Phase::Probe => self.handle_probe(message).await,
            Phase::Modern => self.handle_modern_pinned(message),
            Phase::Legacy => self.handle_legacy(message),
        }
    }

    async fn handle_opening(&mut self, message: Incoming) {
        match classify(&message) {
            Classification::Legacy => {
                self.phase = Phase::Legacy;
                self.handle_legacy(message);
            }
            Classification::Modern => {
                if message.method == "server/discover" {
                    self.phase = Phase::Probe;
                } else {
                    self.phase = Phase::Modern;
                }
                self.handle_modern(message);
            }
            Classification::InvalidEnvelope(issue) => {
                if message.is_request() {
                    self.send_invalid_envelope(message.id.as_ref().unwrap(), &issue);
                }
            }
            Classification::Unsupported(requested) => {
                if message.is_request() {
                    self.send_unsupported(message.id.as_ref().unwrap(), &requested);
                }
            }
        }
    }

    async fn handle_probe(&mut self, message: Incoming) {
        match classify(&message) {
            Classification::Legacy => {
                // A legacy client arrived while the discover probe was open:
                // discard the probe and serve the handshake.
                self.phase = Phase::Legacy;
                self.handle_legacy(message);
            }
            Classification::Modern => {
                if message.is_request() && message.method != "server/discover" {
                    self.phase = Phase::Modern;
                }
                self.handle_modern(message);
            }
            Classification::InvalidEnvelope(issue) => {
                if message.is_request() {
                    self.send_invalid_envelope(message.id.as_ref().unwrap(), &issue);
                }
            }
            Classification::Unsupported(requested) => {
                if message.is_request() {
                    self.send_unsupported(message.id.as_ref().unwrap(), &requested);
                }
            }
        }
    }

    fn handle_modern_pinned(&mut self, message: Incoming) {
        // Cancellation is a connection-level control notification: like the
        // SDK's stdio entry, it is honored even without a modern envelope.
        if !message.is_request() && message.method == "notifications/cancelled" {
            self.handle_cancelled(message.params.as_ref());
            return;
        }
        if !message.is_request() {
            return;
        }
        let id = message.id.clone().unwrap();

        if message.method == "initialize" {
            if carries_valid_modern_claim(&message) {
                // The pinned modern instance has no initialize handler.
                self.send_error(&id, METHOD_NOT_FOUND, "Method not found".into(), None);
            } else {
                self.reject_legacy_initialize(&message);
            }
            return;
        }

        if message.method == "subscriptions/listen" {
            self.handle_listen_pinned(&message);
            return;
        }

        // Method support is checked before the envelope on a pinned instance.
        if !matches!(
            message.method.as_str(),
            "server/discover" | "tools/list" | "tools/call"
        ) {
            self.send_error(&id, METHOD_NOT_FOUND, "Method not found".into(), None);
            return;
        }

        let Some(meta) = message.meta() else {
            self.send_error(&id, INVALID_PARAMS, missing_envelope_message(), None);
            return;
        };
        let issues = validate_envelope_pinned(meta);
        if !issues.is_empty() {
            let joined = issues
                .iter()
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            self.send_error(
                &id,
                INVALID_PARAMS,
                format!(
                    "Invalid _meta envelope for protocol revision {PROTOCOL_VERSION}: {joined}"
                ),
                None,
            );
            return;
        }
        // A pinned connection serves the modern era regardless of the string
        // in the per-request envelope's protocolVersion field.
        self.handle_modern(message);
    }

    /// `subscriptions/listen` is intercepted by the stdio entry before the
    /// pinned instance and uses its own envelope error format.
    fn handle_listen_pinned(&mut self, message: &Incoming) {
        let id = message.id.clone().unwrap();
        let missing = Some((
            "_meta".to_string(),
            "the per-request envelope is required on protocol revision 2026-07-28".to_string(),
        ));
        let issue = match message.meta() {
            None => missing,
            Some(meta) if !meta.contains_key(PV_META) => missing,
            Some(meta) => validate_envelope_opening(meta)
                .into_iter()
                .next()
                .map(|issue| (issue.key(), issue.message)),
        };
        if let Some((key, problem)) = issue {
            self.send_error(
                &id,
                INVALID_PARAMS,
                format!("Invalid _meta envelope: {key}: {problem}"),
                None,
            );
            return;
        }
        match message.claimed_version() {
            Some(version) if SUPPORTED_MODERN_VERSIONS.contains(&version) => {}
            Some(version) => {
                self.send_unsupported(&id, version);
                return;
            }
            None => {
                self.send_unsupported(&id, "unknown");
                return;
            }
        }
        self.handle_listen(id, message.params.as_ref());
    }

    fn handle_modern(&mut self, message: Incoming) {
        let id = match &message.id {
            Some(id) => id.clone(),
            None => {
                // Notifications: cancellation is honored; everything else is
                // dropped (this server never emits list-change events).
                if message.method == "notifications/cancelled" {
                    self.handle_cancelled(message.params.as_ref());
                }
                return;
            }
        };

        match message.method.as_str() {
            "server/discover" => {
                let mut result = Map::new();
                result.insert(
                    "supportedVersions".into(),
                    Value::Array(vec![Value::String(PROTOCOL_VERSION.into())]),
                );
                result.insert("capabilities".into(), json!({"tools": {}}));
                result.insert("resultType".into(), Value::String("complete".into()));
                result.insert("ttlMs".into(), json!(0));
                result.insert("cacheScope".into(), Value::String("private".into()));
                result.insert("_meta".into(), server_info_meta());
                self.send_result(&id, Value::Object(result));
            }
            "tools/list" => match validate_cursor(message.params.as_ref()) {
                Err(issues) => self.send_error(&id, INTERNAL_ERROR, format_issues(&issues), None),
                Ok(()) => {
                    let mut result = Map::new();
                    result.insert("tools".into(), tools_list());
                    result.insert("resultType".into(), Value::String("complete".into()));
                    result.insert("ttlMs".into(), json!(TOOLS_LIST_TTL_MS));
                    result.insert("cacheScope".into(), Value::String("public".into()));
                    result.insert("_meta".into(), server_info_meta());
                    self.send_result(&id, Value::Object(result));
                }
            },
            "tools/call" => match validate_tools_call(message.params.as_ref(), true) {
                Err(issues) => self.send_error(
                    &id,
                    INVALID_PARAMS,
                    format!("Invalid tools/call request: {}", format_issues(&issues)),
                    None,
                ),
                Ok(params) => {
                    if invalid_request_state(message.params.as_ref()) {
                        self.send_error(
                            &id,
                            INVALID_PARAMS,
                            "Invalid or expired requestState".into(),
                            Some(json!({"reason": "invalid_request_state"})),
                        );
                    } else {
                        self.dispatch_tool_call(id, params, true);
                    }
                }
            },
            "subscriptions/listen" => self.handle_listen(id, message.params.as_ref()),
            _ => self.send_error(&id, METHOD_NOT_FOUND, "Method not found".into(), None),
        }
    }

    fn handle_legacy(&mut self, message: Incoming) {
        let id = match &message.id {
            Some(id) => id.clone(),
            None => {
                if message.method == "notifications/cancelled" {
                    self.handle_cancelled(message.params.as_ref());
                }
                return;
            }
        };

        match message.method.as_str() {
            "initialize" => match validate_initialize(message.params.as_ref()) {
                Err(issues) => self.send_error(&id, INTERNAL_ERROR, format_issues(&issues), None),
                Ok(requested) => {
                    let version = negotiate_legacy_version(&requested);
                    let mut result = Map::new();
                    result.insert("protocolVersion".into(), Value::String(version.to_string()));
                    result.insert("capabilities".into(), json!({"tools": {}}));
                    result.insert("serverInfo".into(), server_info());
                    self.send_result(&id, Value::Object(result));
                }
            },
            "ping" => self.send_result(&id, json!({})),
            "tools/list" => match validate_cursor(message.params.as_ref()) {
                Err(issues) => self.send_error(&id, INTERNAL_ERROR, format_issues(&issues), None),
                Ok(()) => self.send_result(&id, json!({"tools": tools_list()})),
            },
            "tools/call" => match validate_tools_call(message.params.as_ref(), false) {
                Err(issues) => self.send_error(
                    &id,
                    INVALID_PARAMS,
                    format!("Invalid tools/call request: {}", format_issues(&issues)),
                    None,
                ),
                Ok(params) => {
                    if invalid_request_state(message.params.as_ref()) {
                        self.send_error(
                            &id,
                            INVALID_PARAMS,
                            "Invalid or expired requestState".into(),
                            Some(json!({"reason": "invalid_request_state"})),
                        );
                    } else {
                        self.dispatch_tool_call(id, params, false);
                    }
                }
            },
            _ => self.send_error(&id, METHOD_NOT_FOUND, "Method not found".into(), None),
        }
    }

    fn reject_legacy_initialize(&self, message: &Incoming) {
        let id = message.id.as_ref().unwrap();
        let requested = message
            .params_map()
            .and_then(|params| params.get("protocolVersion"))
            .and_then(|value| value.as_str());
        match requested {
            Some(version) => self.send_error(
                id,
                UNSUPPORTED_PROTOCOL_VERSION,
                format!("Unsupported protocol version: {version}"),
                Some(json!({
                    "supported": SUPPORTED_MODERN_VERSIONS,
                    "requested": version,
                })),
            ),
            None => self.send_error(
                id,
                UNSUPPORTED_PROTOCOL_VERSION,
                "Unsupported protocol version: the request did not name a protocol version".into(),
                Some(json!({"supported": SUPPORTED_MODERN_VERSIONS})),
            ),
        }
    }

    fn send_invalid_envelope(&self, id: &Value, issue: &SchemaIssue) {
        let key = issue.key();
        let problem = issue.message.clone();
        let mut envelope = Map::new();
        envelope.insert("key".into(), Value::String(key.clone()));
        envelope.insert("problem".into(), Value::String(problem.clone()));
        let mut data = Map::new();
        data.insert("envelope".into(), Value::Object(envelope));
        self.send_error(
            id,
            INVALID_PARAMS,
            format!(
                "Invalid _meta envelope for protocol revision {PROTOCOL_VERSION}: {key}: {problem}"
            ),
            Some(Value::Object(data)),
        );
    }

    fn send_unsupported(&self, id: &Value, requested: &str) {
        self.send_error(
            id,
            UNSUPPORTED_PROTOCOL_VERSION,
            format!("Unsupported protocol version: {requested}"),
            Some(json!({
                "supported": SUPPORTED_MODERN_VERSIONS,
                "requested": requested,
            })),
        );
    }

    fn handle_cancelled(&mut self, params: Option<&Value>) {
        let Some(request_id) = params
            .and_then(|params| params.as_object())
            .and_then(|params| params.get("requestId"))
        else {
            return;
        };
        let key = normalize_id(request_id).to_string();
        self.subscriptions.shift_remove(&key);
        // The SDK's cancellation handler bails on falsy request ids (0, "").
        if is_truthy_request_id(request_id) {
            if let Some(token) = self.inflight.lock().unwrap().get(&key) {
                token.cancel();
            }
        }
    }

    fn handle_listen(&mut self, id: Value, params: Option<&Value>) {
        // Capacity is checked before filter validation (SDK order).
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            self.send_error(
                &id,
                INTERNAL_ERROR,
                "Subscription limit reached".into(),
                None,
            );
            return;
        }
        let filter = params
            .and_then(|params| params.as_object())
            .and_then(|params| params.get("notifications"));
        let Some(filter) = filter else {
            self.send_error(
                &id,
                INVALID_PARAMS,
                "Invalid params: 'notifications' is required and must be a valid SubscriptionFilter"
                    .into(),
                None,
            );
            return;
        };
        if !is_valid_subscription_filter(filter) {
            self.send_error(
                &id,
                INVALID_PARAMS,
                "Invalid params: 'notifications' is required and must be a valid SubscriptionFilter"
                    .into(),
                None,
            );
            return;
        }

        // This server advertises only `tools` with no `listChanged`, so every
        // requested change type narrows away to an empty honored filter.
        let key = id.to_string();
        self.subscriptions.insert(key, id.clone());

        let mut meta = Map::new();
        meta.insert(SUBSCRIPTION_ID_META.into(), id);
        let mut params = Map::new();
        params.insert("notifications".into(), json!({}));
        params.insert("_meta".into(), Value::Object(meta));
        self.send_notification(
            "notifications/subscriptions/acknowledged",
            Value::Object(params),
        );
    }

    fn dispatch_tool_call(&mut self, id: Value, params: ToolCallParams, modern: bool) {
        let tool = params.name.clone();
        match tool.as_str() {
            "google_web_search" => match self.deps.search.parse_web_args(params.arguments.as_ref())
            {
                Ok(args) => self.spawn_search(id, SearchTask::Web(args), modern),
                Err(err) => {
                    self.deps.logger.error(
                        "tool call failed",
                        json!({"tool": tool, "error": err.raw_message()}),
                    );
                    self.send_tool_result(&id, err.user_message(), true, modern);
                }
            },
            "google_image_search" => {
                match self.deps.search.parse_image_args(params.arguments.as_ref()) {
                    Ok(args) => self.spawn_search(id, SearchTask::Image(args), modern),
                    Err(err) => {
                        self.deps.logger.error(
                            "tool call failed",
                            json!({"tool": tool, "error": err.raw_message()}),
                        );
                        self.send_tool_result(&id, err.user_message(), true, modern);
                    }
                }
            }
            _ => self.send_tool_result(&id, format!("Unknown tool: {tool}"), true, modern),
        }
    }

    fn send_tool_result(&self, id: &Value, text: String, is_error: bool, modern: bool) {
        self.send_result(id, tool_result_value(text, is_error, modern));
    }

    fn spawn_search(&self, id: Value, task: SearchTask, modern: bool) {
        let key = id.to_string();
        let request_token = CancellationToken::new();
        self.inflight
            .lock()
            .unwrap()
            .insert(key.clone(), request_token.clone());

        let search = self.deps.search.clone();
        let logger = self.deps.logger.clone();
        let writer = self.writer.clone();
        let shutdown = self.shutdown.clone();
        let inflight = self.inflight.clone();
        let cancel = Cancel::with_request_token(shutdown.clone(), request_token.clone());
        let tool = task.tool_name();

        tokio::spawn(async move {
            // The shared upstream must run even if this request was
            // cancelled: other coalesced waiters may depend on its result.
            let result = match task {
                SearchTask::Web(args) => search.web_search(&args, cancel).await,
                SearchTask::Image(args) => search.image_search(&args, cancel).await,
            };

            inflight.lock().unwrap().remove(&key);

            if request_token.is_cancelled() || shutdown.is_cancelled() {
                return;
            }

            let (text, is_error) = match result {
                Ok(text) => (text, false),
                Err(err) => {
                    logger.error(
                        "tool call failed",
                        json!({"tool": tool, "error": err.raw_message()}),
                    );
                    (err.user_message(), true)
                }
            };

            let mut message = Map::new();
            message.insert("result".into(), tool_result_value(text, is_error, modern));
            message.insert("jsonrpc".into(), Value::String("2.0".into()));
            message.insert("id".into(), id);
            if let Ok(line) = serde_json::to_string(&Value::Object(message)) {
                let _ = writer.send(line);
            }
        });
    }

    /// Emit graceful-close results for every active listen subscription.
    fn teardown_subscriptions(&mut self) {
        let subscriptions: Vec<Value> = self.subscriptions.drain(..).map(|(_, id)| id).collect();
        for id in subscriptions {
            let mut meta = Map::new();
            meta.insert(SUBSCRIPTION_ID_META.into(), id.clone());
            meta.insert(SERVER_INFO_META.into(), server_info());
            let mut result = Map::new();
            result.insert("resultType".into(), Value::String("complete".into()));
            result.insert("_meta".into(), Value::Object(meta));
            // The SDK's graceful-close result is stamped as jsonrpc/id/result
            // (unlike ordinary handler results, which serialize result-first).
            let mut message = Map::new();
            message.insert("jsonrpc".into(), Value::String("2.0".into()));
            message.insert("id".into(), id);
            message.insert("result".into(), Value::Object(result));
            self.send(Value::Object(message));
        }
    }
}

fn is_valid_subscription_filter(filter: &Value) -> bool {
    let Some(map) = filter.as_object() else {
        return false;
    };
    for field in [
        "toolsListChanged",
        "promptsListChanged",
        "resourcesListChanged",
    ] {
        if let Some(value) = map.get(field) {
            if !value.is_boolean() {
                return false;
            }
        }
    }
    if let Some(value) = map.get("resourceSubscriptions") {
        let Some(items) = value.as_array() else {
            return false;
        };
        if items.iter().any(|item| !item.is_string()) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Connection runner
// ---------------------------------------------------------------------------

/// Run the stdio server until stdin closes or a shutdown signal arrives.
/// Returns the process exit code.
pub async fn run(deps: ServerDeps, shutdown: ShutdownState) -> i32 {
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<ShutdownRequest>();

    // stderr died: stop logging and shut down the connection.
    {
        let tx = shutdown_tx.clone();
        crate::logger::set_dead_hook(Box::new(move |message| {
            let _ = tx.send(ShutdownRequest {
                reason: format!("stdio stderr failed: {message}"),
                code: 0,
            });
        }));
    }

    let writer_shutdown = shutdown_tx.clone();
    let writer_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = writer_rx.recv().await {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if let Err(err) = stdout.write_all(&bytes).await {
                crate::logger::silence_logger();
                let _ = writer_shutdown.send(ShutdownRequest {
                    reason: format!("stdio stdout failed: {err}"),
                    code: 0,
                });
                break;
            }
            if let Err(err) = stdout.flush().await {
                crate::logger::silence_logger();
                let _ = writer_shutdown.send(ShutdownRequest {
                    reason: format!("stdio stdout failed: {err}"),
                    code: 0,
                });
                break;
            }
        }
    });

    spawn_signal_handlers(shutdown_tx.clone());
    spawn_prune_timer(deps.cache.clone(), deps.logger.clone());

    deps.logger.info(
        "Google Search MCP Server running on stdio (2026-07-28 + legacy fallback)",
        json!({
            "perSecondLimit": deps.limiter.stats().per_second_limit,
            "dailyLimit": deps.limiter.stats().daily_limit,
            "cacheTtlMs": deps.cache_ttl_ms,
        }),
    );

    let mut connection = Connection::new(deps.clone(), writer_tx.clone(), shutdown.clone());
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    let mut exit_reason = String::from("stdin closed");
    let mut exit_code = 0;

    loop {
        // Drain complete lines before reading more.
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=position).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                if let Some(message) = Incoming::parse(value) {
                    connection.handle(message).await;
                }
            }
        }

        tokio::select! {
            request = shutdown_rx.recv() => {
                if let Some(request) = request {
                    exit_reason = request.reason;
                    exit_code = request.code;
                }
                break;
            }
            read = reader.read(&mut chunk) => {
                match read {
                    Ok(0) => {
                        exit_reason = "stdin closed".into();
                        break;
                    }
                    Ok(count) => {
                        buffer.extend_from_slice(&chunk[..count]);
                        // The SDK checks the cap before splitting lines: the
                        // chunk that overflows is rejected in full.
                        if buffer.len() > STDIO_MAX_BUFFER_SIZE {
                            exit_reason = format!(
                                "stdin error: ReadBuffer exceeded maximum size of {STDIO_MAX_BUFFER_SIZE} bytes"
                            );
                            break;
                        }
                    }
                    Err(err) => {
                        exit_reason = format!("stdin error: {err}");
                        break;
                    }
                }
            }
        }
    }

    deps.logger.info(
        "shutting down",
        json!({
            "reason": exit_reason,
            "cache": deps.cache.stats(),
            "rate": deps.limiter.stats(),
        }),
    );

    // Hard bound first: graceful teardown must never hang the process.
    let watchdog_code = exit_code;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        std::process::exit(watchdog_code);
    });

    shutdown.cancel(format!("server shutting down: {exit_reason}"));
    connection.teardown_subscriptions();

    drop(connection);
    drop(writer_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(1_400), writer_task).await;
    crate::logger::flush();

    exit_code
}

fn spawn_signal_handlers(tx: mpsc::UnboundedSender<ShutdownRequest>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut interrupt) = signal(SignalKind::interrupt()) {
            let tx = tx.clone();
            tokio::spawn(async move {
                interrupt.recv().await;
                let _ = tx.send(ShutdownRequest {
                    reason: "SIGINT".into(),
                    code: 0,
                });
            });
        }
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            let tx = tx.clone();
            tokio::spawn(async move {
                terminate.recv().await;
                let _ = tx.send(ShutdownRequest {
                    reason: "SIGTERM".into(),
                    code: 0,
                });
            });
        }
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = tx.send(ShutdownRequest {
                    reason: "SIGINT".into(),
                    code: 0,
                });
            }
        });
    }
}

fn spawn_prune_timer(cache: Arc<TtlLruCache>, logger: Logger) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.tick().await; // first tick completes immediately
        loop {
            interval.tick().await;
            let removed = cache.prune();
            if removed > 0 {
                let stats = cache.stats();
                let mut fields = Map::new();
                fields.insert("removed".into(), json!(removed));
                fields.insert("hits".into(), json!(stats.hits));
                fields.insert("misses".into(), json!(stats.misses));
                fields.insert("evictions".into(), json!(stats.evictions));
                fields.insert("expirations".into(), json!(stats.expirations));
                fields.insert("size".into(), json!(stats.size));
                fields.insert("maxEntries".into(), json!(stats.max_entries));
                logger.debug("cache pruned", Value::Object(fields));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(value: Value) -> Incoming {
        Incoming::parse(value).expect("valid message")
    }

    fn envelope(extra: Value) -> Value {
        let mut meta = Map::new();
        meta.insert(PV_META.into(), Value::String(PROTOCOL_VERSION.into()));
        meta.insert(CLIENT_CAPABILITIES_META.into(), Value::Object(Map::new()));
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                meta.insert(key, value);
            }
        }
        Value::Object(meta)
    }

    #[test]
    fn tools_are_byte_stable() {
        let tools = tools_list();
        assert_eq!(tools.as_array().unwrap().len(), 2);
        assert_eq!(tools[0]["name"], "google_web_search");
        assert!(tools[0]["description"]
            .as_str()
            .unwrap()
            .ends_with("pagination. "));
        assert_eq!(tools[1]["inputSchema"]["required"][0], "query");
    }

    #[test]
    fn classify_modern_and_legacy() {
        let modern = incoming(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": envelope(json!({}))}
        }));
        assert!(matches!(classify(&modern), Classification::Modern));

        let legacy = incoming(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        }));
        assert!(matches!(classify(&legacy), Classification::Legacy));

        let bad = incoming(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": {PV_META: "1999-01-01", CLIENT_CAPABILITIES_META: {}}}
        }));
        match classify(&bad) {
            Classification::Unsupported(version) => assert_eq!(version, "1999-01-01"),
            _ => panic!("expected unsupported"),
        }

        let missing = incoming(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": {PV_META: PROTOCOL_VERSION}}
        }));
        match classify(&missing) {
            Classification::InvalidEnvelope(issue) => {
                assert_eq!(issue.key(), CLIENT_CAPABILITIES_META);
                assert_eq!(issue.message, "missing");
            }
            _ => panic!("expected invalid envelope"),
        }
    }

    #[test]
    fn initialize_with_modern_envelope_is_modern() {
        let message = incoming(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}, "_meta": envelope(json!({}))}
        }));
        assert!(matches!(classify(&message), Classification::Modern));
    }

    #[test]
    fn rejects_malformed_json_rpc_shapes() {
        assert!(Incoming::parse(json!({"jsonrpc":"2.0","id":null,"method":"ping"})).is_none());
        assert!(Incoming::parse(json!({"jsonrpc":"2.0","id":1.5,"method":"ping"})).is_none());
        assert!(
            Incoming::parse(json!({"jsonrpc":"2.0","id":1,"method":"ping","extra":1})).is_none()
        );
        assert!(Incoming::parse(json!({"jsonrpc":"1.0","id":1,"method":"ping"})).is_none());
        assert!(
            Incoming::parse(json!({"jsonrpc":"2.0","id":1,"method":"ping","params":[]})).is_none()
        );
        assert!(
            Incoming::parse(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_some()
        );
    }

    #[test]
    fn validates_tools_call_like_sdk() {
        let issues = validate_tools_call(None, true).unwrap_err();
        assert!(format_issues(&issues).contains("\"expected\": \"object\""));
        let issues = validate_tools_call(Some(&json!({"arguments": null})), true).unwrap_err();
        let text = format_issues(&issues);
        assert!(text.contains("\"expected\": \"string\""));
        assert!(text.contains("\"path\": ["));
        assert!(text.contains("expected record, received null"));
    }

    #[test]
    fn nested_envelope_shapes_are_rejected() {
        let meta = json!({
            PV_META: PROTOCOL_VERSION,
            CLIENT_CAPABILITIES_META: {"sampling": {"tools": 5}}
        });
        let issues = validate_envelope_opening(meta.as_object().unwrap());
        assert_eq!(
            issues[0].key(),
            "io.modelcontextprotocol/clientCapabilities.sampling.tools"
        );
        assert_eq!(
            issues[0].message,
            "Invalid input: expected record, received number"
        );

        let meta = json!({
            PV_META: PROTOCOL_VERSION,
            CLIENT_INFO_META: {"name": "s", "version": "1", "icons": 5},
            CLIENT_CAPABILITIES_META: {}
        });
        let issues = validate_envelope_opening(meta.as_object().unwrap());
        assert_eq!(issues[0].key(), "io.modelcontextprotocol/clientInfo.icons");
        assert_eq!(
            issues[0].message,
            "Invalid input: expected array, received number"
        );
    }

    #[test]
    fn pinned_envelope_joins_messages_without_keys() {
        let meta = json!({PV_META: 5, CLIENT_INFO_META: {"name": "s"}});
        let issues = validate_envelope_pinned(meta.as_object().unwrap());
        let joined = issues
            .iter()
            .map(|issue| issue.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        assert_eq!(
            joined,
            "Invalid input: expected string, received number; Invalid input: expected string, received undefined; Invalid input: expected object, received undefined"
        );
    }

    #[test]
    fn legacy_version_negotiation() {
        assert_eq!(negotiate_legacy_version("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate_legacy_version("2024-10-07"), "2024-10-07");
        assert_eq!(negotiate_legacy_version("1999-01-01"), "2025-11-25");
        assert_eq!(negotiate_legacy_version(PROTOCOL_VERSION), "2025-11-25");
    }

    #[test]
    fn transport_meta_shape_is_enforced() {
        assert!(!is_valid_transport_meta(
            json!({"_meta": 5}).as_object().unwrap()
        ));
        assert!(!is_valid_transport_meta(
            json!({"_meta": {"progressToken": 1.5}})
                .as_object()
                .unwrap()
        ));
        assert!(!is_valid_transport_meta(
            json!({"_meta": {"io.modelcontextprotocol/related-task": {"taskId": 5}}})
                .as_object()
                .unwrap()
        ));
        assert!(is_valid_transport_meta(
            json!({"_meta": {"progressToken": 7}}).as_object().unwrap()
        ));
    }

    #[test]
    fn legacy_task_and_request_state_validation() {
        let issues = validate_tools_call(
            Some(&json!({"name": "google_web_search", "task": 5})),
            false,
        )
        .unwrap_err();
        assert!(format_issues(&issues).contains("\"path\": [\n      \"params\",\n      \"task\""));
        assert!(validate_tools_call(Some(&json!({"name": "x", "task": 5})), true).is_ok());
        assert!(invalid_request_state(Some(&json!({"requestState": 5}))));
        assert!(invalid_request_state(Some(&json!({"requestState": null}))));
        assert!(!invalid_request_state(Some(
            &json!({"requestState": "state"})
        )));
        assert!(!invalid_request_state(Some(&json!({}))));
    }

    #[test]
    fn validates_cursor() {
        assert!(validate_cursor(Some(&json!({"cursor": "x"}))).is_ok());
        let issues = validate_cursor(Some(&json!({"cursor": 5}))).unwrap_err();
        assert!(format_issues(&issues).contains("received number"));
    }

    #[test]
    fn tool_list_modern_shape() {
        let mut result = Map::new();
        result.insert("tools".into(), tools_list());
        result.insert("resultType".into(), Value::String("complete".into()));
        result.insert("ttlMs".into(), json!(TOOLS_LIST_TTL_MS));
        result.insert("cacheScope".into(), Value::String("public".into()));
        result.insert("_meta".into(), server_info_meta());
        let text = Value::Object(result).to_string();
        assert!(text.starts_with("{\"tools\":"));
        assert!(text.contains("\"resultType\":\"complete\""));
        assert!(text.contains("\"cacheScope\":\"public\""));
        assert!(text.contains(SERVER_INFO_META));
    }

    #[test]
    fn iso_reset_format() {
        assert_eq!(
            crate::util::format_iso_ms(86_400_000),
            "1970-01-02T00:00:00.000Z"
        );
    }
}
