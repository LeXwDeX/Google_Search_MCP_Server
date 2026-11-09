//! Cancellation shared by HTTP requests, the rate limiter and tool calls.
//!
//! A request is cancelled either by the client (`notifications/cancelled`) or
//! by server shutdown. The shutdown reason is preserved so error text matches
//! the TypeScript build (`server shutting down: <reason>`).

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ShutdownState {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl Default for ShutdownState {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownState {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(Mutex::new(None)),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|guard| guard.clone())
    }

    /// Record the reason (first writer wins) and cancel every request.
    pub fn cancel(&self, reason: String) {
        if let Ok(mut guard) = self.reason.lock() {
            if guard.is_none() {
                *guard = Some(reason);
            }
        }
        self.token.cancel();
    }
}

/// Per-request cancellation handle: client cancellation plus shutdown.
#[derive(Clone)]
pub struct Cancel {
    request: CancellationToken,
    shutdown: ShutdownState,
}

impl Cancel {
    pub fn new(shutdown: ShutdownState) -> Self {
        Self {
            request: CancellationToken::new(),
            shutdown,
        }
    }

    /// Build a handle around an already-created per-request token.
    pub fn with_request_token(shutdown: ShutdownState, request: CancellationToken) -> Self {
        Self { request, shutdown }
    }

    pub fn request_token(&self) -> CancellationToken {
        self.request.clone()
    }

    pub fn is_request_cancelled(&self) -> bool {
        self.request.is_cancelled()
    }

    pub fn is_cancelled(&self) -> bool {
        self.request.is_cancelled() || self.shutdown.is_cancelled()
    }

    pub fn reason(&self) -> String {
        if self.shutdown.is_cancelled() {
            if let Some(reason) = self.shutdown.reason() {
                return reason;
            }
        }
        "request cancelled".to_string()
    }

    /// Resolves when either cancellation source fires.
    pub async fn cancelled(&self) {
        let shutdown_token = self.shutdown.token();
        tokio::select! {
            _ = self.request.cancelled() => {}
            _ = shutdown_token.cancelled() => {}
        }
    }
}
