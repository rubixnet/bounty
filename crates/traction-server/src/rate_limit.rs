//! A per-device token bucket. Time is passed in, so it is testable without
//! sleeping; a refusal carries the whole seconds to put in `Retry-After`.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    /// The time the bucket was last topped up.
    at_ms: u64,
}

pub struct RateLimiter {
    burst: u32,
    refill_ms: u64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(burst: u32, refill_ms: u64) -> Self {
        Self {
            burst: burst.max(1),
            refill_ms: refill_ms.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `device`, or returns the seconds to wait.
    pub fn check(&self, device: &str, now_ms: u64) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets.entry(device.to_owned()).or_insert(Bucket {
            tokens: self.burst,
            at_ms: now_ms,
        });
        let gained = now_ms.saturating_sub(bucket.at_ms) / self.refill_ms;
        if gained > 0 {
            let topped = u64::from(bucket.tokens).saturating_add(gained);
            bucket.tokens = u32::try_from(topped.min(u64::from(self.burst))).unwrap_or(self.burst);
            bucket.at_ms += gained * self.refill_ms;
        }
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            return Ok(());
        }
        let next_ms = bucket.at_ms + self.refill_ms;
        Err(next_ms.saturating_sub(now_ms).div_ceil(1000).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_then_a_refusal_with_retry_after() {
        let limiter = RateLimiter::new(3, 2_000);
        for _ in 0..3 {
            assert_eq!(limiter.check("d", 10_000), Ok(()));
        }
        assert_eq!(limiter.check("d", 10_000), Err(2));
        // Another device has its own bucket.
        assert_eq!(limiter.check("e", 10_000), Ok(()));
    }

    #[test]
    fn tokens_come_back_with_time_up_to_the_burst() {
        let limiter = RateLimiter::new(2, 1_000);
        assert_eq!(limiter.check("d", 0), Ok(()));
        assert_eq!(limiter.check("d", 0), Ok(()));
        assert!(limiter.check("d", 500).is_err());
        assert_eq!(limiter.check("d", 1_000), Ok(()));
        // A long idle never banks more than the burst.
        assert_eq!(limiter.check("d", 1_000_000), Ok(()));
        assert_eq!(limiter.check("d", 1_000_000), Ok(()));
        assert!(limiter.check("d", 1_000_000).is_err());
    }
}
