//! Rate limiting and daily quota enforcement.
//!
//! Two independent guards:
//!  1. A per-second token bucket that smooths burst traffic. When no token is
//!     available the caller *waits* (up to a configurable ceiling) instead of
//!     failing immediately.
//!  2. A hard daily quota that mirrors the Google free tier. When exhausted
//!     there is nothing to wait for, so acquisition fails fast.
//!
//! Acquisitions are serialized through an async mutex, while the bucket state
//! itself lives behind a short synchronous lock so `stats()` never blocks on a
//! sleeping acquisition.

use crate::cancel::Cancel;
use crate::error::AppError;
use crate::util::{format_iso_ms, now_ms, start_of_utc_day};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

struct State {
    tokens: f64,
    last_refill: i64,
    daily_used: u32,
    day_start: i64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimiterStats {
    pub daily_used: u32,
    pub daily_limit: u32,
    pub daily_reset_at: i64,
    pub per_second_limit: u32,
}

pub struct RateLimiter {
    per_second_limit: u32,
    daily_limit: u32,
    max_wait_ms: u64,
    now: NowFn,
    state: Mutex<State>,
    /// Serializes acquire() so concurrent callers queue deterministically.
    chain: tokio::sync::Mutex<()>,
}

impl RateLimiter {
    pub fn new(per_second_limit: u32, daily_limit: u32, max_wait_ms: u64) -> Self {
        Self::with_clock(per_second_limit, daily_limit, max_wait_ms, Arc::new(now_ms))
    }

    pub fn with_clock(
        per_second_limit: u32,
        daily_limit: u32,
        max_wait_ms: u64,
        now: NowFn,
    ) -> Self {
        let now_value = now();
        Self {
            per_second_limit,
            daily_limit,
            max_wait_ms,
            now,
            state: Mutex::new(State {
                tokens: per_second_limit as f64,
                last_refill: now_value,
                daily_used: 0,
                day_start: start_of_utc_day(now_value),
            }),
            chain: tokio::sync::Mutex::new(()),
        }
    }

    fn refill(state: &mut State, per_second_limit: u32, now: i64) {
        let elapsed = now - state.last_refill;
        if elapsed <= 0 {
            return;
        }
        let refill = (elapsed as f64 / 1000.0) * per_second_limit as f64;
        if refill >= 1.0 {
            state.tokens = (state.tokens + refill.floor()).min(per_second_limit as f64);
            state.last_refill = now;
        }
    }

    fn roll_day(state: &mut State, now: i64) {
        let today = start_of_utc_day(now);
        if today > state.day_start {
            state.day_start = today;
            state.daily_used = 0;
        }
    }

    /// Acquire one unit of permission. Waits for a per-second token, fails with
    /// the typed quota error when the day is exhausted, or with the typed
    /// timeout error when the wait would exceed the configured ceiling.
    pub async fn acquire(&self, cancel: &Cancel) -> Result<(), AppError> {
        let _chain = self.chain.lock().await;

        {
            let now = (self.now)();
            let mut state = self.state.lock().unwrap();
            Self::roll_day(&mut state, now);
            if state.daily_used >= self.daily_limit {
                let reset_at = state.day_start + 86_400_000;
                return Err(AppError::QuotaExceeded {
                    message: format!(
                        "Daily quota exhausted ({}/{}). Resets at {}.",
                        state.daily_used,
                        self.daily_limit,
                        format_iso_ms(reset_at)
                    ),
                    used: state.daily_used,
                    limit: self.daily_limit,
                    reset_at,
                });
            }
        }

        let deadline = (self.now)() + self.max_wait_ms as i64;
        loop {
            {
                let now = (self.now)();
                let mut state = self.state.lock().unwrap();
                Self::refill(&mut state, self.per_second_limit, now);
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    state.daily_used += 1;
                    return Ok(());
                }
            }

            let ms_per_token = 1000.0 / self.per_second_limit as f64;
            let mut wait = ms_per_token.ceil() as i64;
            let remaining = deadline - (self.now)();
            if remaining <= 0 {
                return Err(AppError::RateLimitTimeout(format!(
                    "Rate limit wait exceeded {}ms (per-second limit {}).",
                    self.max_wait_ms, self.per_second_limit
                )));
            }
            if wait > remaining {
                wait = remaining;
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait.max(0) as u64)) => {}
                _ = cancel.cancelled() => return Err(AppError::Aborted(cancel.reason())),
            }
        }
    }

    pub fn stats(&self) -> RateLimiterStats {
        let now = (self.now)();
        let mut state = self.state.lock().unwrap();
        Self::roll_day(&mut state, now);
        RateLimiterStats {
            daily_used: state.daily_used,
            daily_limit: self.daily_limit,
            daily_reset_at: state.day_start + 86_400_000,
            per_second_limit: self.per_second_limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::ShutdownState;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn limiter(per_second: u32, daily: u32, max_wait: u64) -> (RateLimiter, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(1_000));
        let handle = clock.clone();
        let limiter = RateLimiter::with_clock(
            per_second,
            daily,
            max_wait,
            Arc::new(move || handle.load(Ordering::SeqCst)),
        );
        (limiter, clock)
    }

    #[tokio::test]
    async fn consumes_bucket_then_hits_daily_quota() {
        let (limiter, _) = limiter(2, 2, 0);
        let cancel = Cancel::new(ShutdownState::new());
        assert!(limiter.acquire(&cancel).await.is_ok());
        assert!(limiter.acquire(&cancel).await.is_ok());
        match limiter.acquire(&cancel).await {
            Err(AppError::QuotaExceeded { used, limit, .. }) => {
                assert_eq!((used, limit), (2, 2));
            }
            other => panic!("expected quota error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_timeout_when_no_wait_budget() {
        let (limiter, _) = limiter(1, 10, 0);
        let cancel = Cancel::new(ShutdownState::new());
        assert!(limiter.acquire(&cancel).await.is_ok());
        match limiter.acquire(&cancel).await {
            Err(AppError::RateLimitTimeout(message)) => {
                assert!(message.contains("Rate limit wait exceeded 0ms"));
            }
            other => panic!("expected rate timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stats_roll_day_at_utc_midnight() {
        let (limiter, clock) = limiter(5, 10, 0);
        let cancel = Cancel::new(ShutdownState::new());
        limiter.acquire(&cancel).await.unwrap();
        assert_eq!(limiter.stats().daily_used, 1);
        clock.store(86_400_000 + 5_000, Ordering::SeqCst);
        assert_eq!(limiter.stats().daily_used, 0);
        assert_eq!(limiter.stats().daily_reset_at, 172_800_000);
    }

    #[tokio::test]
    async fn cancellation_interrupts_wait() {
        use std::sync::Arc;
        let state = ShutdownState::new();
        let (limiter, _) = limiter(1, 10, 10_000);
        let limiter = Arc::new(limiter);
        let cancel = Cancel::new(state.clone());
        limiter.acquire(&cancel).await.unwrap();

        let waiter = {
            let limiter = limiter.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { limiter.acquire(&cancel).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        state.cancel("server shutting down: test".into());
        match waiter.await.unwrap() {
            Err(AppError::Aborted(reason)) => {
                assert_eq!(reason, "server shutting down: test");
            }
            other => panic!("expected abort, got {other:?}"),
        }
    }
}
