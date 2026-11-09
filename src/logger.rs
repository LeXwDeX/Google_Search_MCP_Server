//! Leveled, structured logger writing JSON lines to stderr.
//!
//! stdout is reserved for the MCP JSON-RPC stream, so every log line goes to
//! stderr. Writes happen on a dedicated thread: a full-but-open stderr pipe
//! can block that thread, but must never block the async runtime (which owns
//! stdin, stdout and signal handling). Once stderr is known-dead (EPIPE after
//! the client crashed), all writes become no-ops.

use serde_json::{Map, Value};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::OnceLock;
use std::time::Duration;

/// Log levels, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Silent,
}

impl LogLevel {
    pub const ALL: [LogLevel; 5] = [
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
        LogLevel::Silent,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Silent => "silent",
        }
    }

    fn weight(self) -> u8 {
        match self {
            LogLevel::Debug => 10,
            LogLevel::Info => 20,
            LogLevel::Warn => 30,
            LogLevel::Error => 40,
            LogLevel::Silent => 100,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "silent" => Some(LogLevel::Silent),
            _ => None,
        }
    }
}

/// True once stderr is known-dead.
static OUTPUT_DEAD: AtomicBool = AtomicBool::new(false);

/// Callback invoked once when a stderr write fails (used to trigger shutdown).
static DEAD_HOOK: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();

enum LogMessage {
    Line(String),
    Flush(SyncSender<()>),
}

static LOG_SENDER: OnceLock<Sender<LogMessage>> = OnceLock::new();

/// Lazily start the dedicated stderr writer thread.
fn sender() -> &'static Sender<LogMessage> {
    LOG_SENDER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<LogMessage>();
        let _ = std::thread::Builder::new()
            .name("mcp-stderr".into())
            .spawn(move || {
                let mut stderr = std::io::stderr().lock();
                while let Ok(message) = rx.recv() {
                    match message {
                        LogMessage::Line(text) => {
                            if OUTPUT_DEAD.load(Ordering::SeqCst) {
                                continue;
                            }
                            if let Err(err) = stderr
                                .write_all(text.as_bytes())
                                .and_then(|_| stderr.flush())
                            {
                                // Flip first: the hook's shutdown path may log again.
                                OUTPUT_DEAD.store(true, Ordering::SeqCst);
                                if let Some(hook) = DEAD_HOOK.get() {
                                    hook(err.to_string());
                                }
                            }
                        }
                        LogMessage::Flush(ack) => {
                            let _ = stderr.flush();
                            let _ = ack.send(());
                        }
                    }
                }
            });
        tx
    })
}

/// Mark stderr as unusable; every future log write becomes a no-op.
pub fn silence_logger() {
    OUTPUT_DEAD.store(true, Ordering::SeqCst);
}

pub fn output_dead() -> bool {
    OUTPUT_DEAD.load(Ordering::SeqCst)
}

/// Install the shutdown hook fired when stderr dies.
pub fn set_dead_hook(hook: Box<dyn Fn(String) + Send + Sync>) {
    let _ = DEAD_HOOK.set(hook);
}

/// Wait (bounded) until queued log lines have reached the writer thread.
pub fn flush() {
    let Some(sender) = LOG_SENDER.get() else {
        return;
    };
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
    if sender.send(LogMessage::Flush(ack_tx)).is_ok() {
        let _ = ack_rx.recv_timeout(Duration::from_millis(250));
    }
}

/// Keys whose values must never be logged in full.
const SENSITIVE_KEYS: &[&str] = &[
    "key",
    "apiKey",
    "api_key",
    "token",
    "authorization",
    "password",
];

fn redact(fields: Map<String, Value>) -> Map<String, Value> {
    fields
        .into_iter()
        .map(|(k, v)| {
            if SENSITIVE_KEYS.contains(&k.as_str()) {
                (k, Value::String("[redacted]".into()))
            } else {
                (k, v)
            }
        })
        .collect()
}

/// A logger with inherited bindings. Cloning is cheap.
#[derive(Clone)]
pub struct Logger {
    level: LogLevel,
    bindings: Map<String, Value>,
}

impl Logger {
    pub fn new(level: LogLevel) -> Self {
        Self {
            level,
            bindings: Map::new(),
        }
    }

    pub fn level(&self) -> LogLevel {
        self.level
    }

    /// Return a child logger with extra bindings merged in.
    pub fn child(&self, extra: Value) -> Self {
        let mut bindings = self.bindings.clone();
        if let Value::Object(map) = extra {
            for (k, v) in map {
                bindings.insert(k, v);
            }
        }
        Self {
            level: self.level,
            bindings,
        }
    }

    pub fn debug(&self, message: &str, fields: Value) {
        self.write(LogLevel::Debug, message, fields);
    }

    pub fn info(&self, message: &str, fields: Value) {
        self.write(LogLevel::Info, message, fields);
    }

    pub fn warn(&self, message: &str, fields: Value) {
        self.write(LogLevel::Warn, message, fields);
    }

    pub fn error(&self, message: &str, fields: Value) {
        self.write(LogLevel::Error, message, fields);
    }

    fn write(&self, level: LogLevel, message: &str, fields: Value) {
        if OUTPUT_DEAD.load(Ordering::SeqCst) || level.weight() < self.level.weight() {
            return;
        }
        let mut merged = self.bindings.clone();
        if let Value::Object(map) = fields {
            for (k, v) in map {
                merged.insert(k, v);
            }
        }

        let mut line = Map::new();
        line.insert("ts".into(), Value::String(crate::util::format_now_iso()));
        line.insert("level".into(), Value::String(level.as_str().into()));
        line.insert("msg".into(), Value::String(message.into()));
        for (k, v) in redact(merged) {
            line.insert(k, v);
        }

        let mut text = serde_json::to_string(&Value::Object(line)).unwrap_or_else(|_| {
            format!(
                "{{\"ts\":\"{}\",\"level\":\"{}\",\"msg\":\"{}\",\"fieldsError\":\"unserializable\"}}",
                crate::util::format_now_iso(),
                level.as_str(),
                message
            )
        });
        text.push('\n');

        // Non-blocking hand-off; a full stderr pipe only stalls the writer
        // thread, never a tokio worker.
        let _ = sender().send(LogMessage::Line(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_and_parse() {
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("WARN"), None);
        assert!(LogLevel::Debug.weight() < LogLevel::Error.weight());
    }
}
