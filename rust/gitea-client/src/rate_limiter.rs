//! Per-IP token bucket rate limiter for webhook requests — stage 16.
//!
//! ## Overview
//!
//! Simple token bucket rate limiter, keyed by client IP. Each bucket starts
//! full at `capacity` tokens and refills continuously at
//! `refill_per_second` tokens / second up to `capacity`. Every incoming
//! webhook costs one token; if the bucket cannot cover the cost, the request
//! is rejected with `429 Too Many Requests`.
//!
//! The intended use case is webhook ingress in [`crate::server`]:
//! protection against DDoS floods and brute-force HMAC probing. A single
//! misbehaving Gitea server (or attacker) cannot starve other clients
//! because buckets are isolated per source IP.
//!
//! ## Concurrency
//!
//! The internal `HashMap` is guarded by a `std::sync::Mutex`. The handler
//! holds the lock only for the duration of a token-bucket update (no I/O,
//! no allocations beyond the rare `HashMap` insert on first sighting of an
//! IP), so contention is negligible for the webhook use case (tens of
//! requests per second at most).
//!
//! ## Stale-entry reaping
//!
//! Buckets for clients that stop sending are lazily kept in the map until
//! [`RateLimiter::cleanup_stale`] is called by a periodic tokio task
//! (every 5 minutes from [`crate::server::WebhookServer::start`]). This
//! bounds memory growth from spoofed-source floods.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token-bucket rate limiter keyed by client IP.
pub struct RateLimiter {
    state: Mutex<HashMap<IpAddr, Bucket>>,
    capacity: u32,
    refill_per_second: f64,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Construct a new limiter.
    ///
    /// * `capacity` — maximum tokens per bucket. Each bucket starts full.
    /// * `refill_per_second` — continuous refill rate in tokens / second.
    ///   A bucket can never hold more than `capacity` tokens.
    pub fn new(capacity: u32, refill_per_second: f64) -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            capacity,
            refill_per_second,
        }
    }

    /// Returns `true` if the request is allowed, `false` if rate-limited.
    ///
    /// Charges one token from the caller's bucket. Idempotent refill is
    /// performed first, so a client that paces itself at <= refill rate
    /// is never throttled.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut state = self.state.lock().expect("rate limiter mutex poisoned");
        let now = Instant::now();
        let bucket = state.entry(ip).or_insert_with(|| Bucket {
            tokens: self.capacity as f64,
            last_refill: now,
        });

        // Refill: continuous token-bucket model. Cap at `capacity` so a
        // client that goes quiet for an hour does not bank more than its
        // burst allowance.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * self.refill_per_second).min(self.capacity as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drop buckets that have been idle longer than `max_idle`.
    ///
    /// Intended to be called from a periodic background task
    /// (see [`crate::server::WebhookServer::start`]). Prevents the map
    /// from growing without bound under a spoofed-source attack.
    pub fn cleanup_stale(&self, max_idle: Duration) {
        let mut state = self.state.lock().expect("rate limiter mutex poisoned");
        let now = Instant::now();
        state.retain(|_, bucket| now.duration_since(bucket.last_refill) < max_idle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn allows_first_burst_then_rejects() {
        // Capacity 3, refill 0/sec ⇒ the bucket starts with 3 tokens and
        // never regenerates, so the first three requests are allowed and
        // the fourth is rejected.
        let rl = RateLimiter::new(3, 0.0);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(!rl.check(ip), "fourth request should be throttled");
    }

    #[test]
    fn per_ip_isolation() {
        // Two different IPs each get their own bucket.
        let rl = RateLimiter::new(1, 0.0);
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        assert!(rl.check(a));
        assert!(rl.check(b));
        assert!(!rl.check(a), "second request from same IP throttled");
        assert!(!rl.check(b));
    }

    #[test]
    fn cleanup_drops_stale_buckets() {
        let rl = RateLimiter::new(1, 0.0);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        assert!(rl.check(ip));

        // Force the bucket to look stale by manually rewriting its
        // `last_refill` timestamp through the internal mutex. This is a
        // white-box test: we touch the inner state directly because the
        // public API does not expose a way to fast-forward time.
        {
            let mut state = rl.state.lock().unwrap();
            let bucket = state.get_mut(&ip).unwrap();
            bucket.last_refill = Instant::now() - Duration::from_secs(600);
        }

        rl.cleanup_stale(Duration::from_secs(60));
        {
            let state = rl.state.lock().unwrap();
            assert!(!state.contains_key(&ip), "stale bucket should be reaped");
        }
    }

    #[test]
    fn cleanup_keeps_active_buckets() {
        let rl = RateLimiter::new(5, 1.0);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        assert!(rl.check(ip));
        rl.cleanup_stale(Duration::from_secs(60));
        {
            let state = rl.state.lock().unwrap();
            assert!(state.contains_key(&ip), "fresh bucket should survive cleanup");
        }
    }
}
