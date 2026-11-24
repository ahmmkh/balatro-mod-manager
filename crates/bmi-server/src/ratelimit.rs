//! In-memory token buckets keyed by (class, client). Fine for a single-process service.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>; // milliseconds, monotonic

struct Bucket {
    tokens: f64,
    at: u64,
}

pub struct RateLimiter {
    now: Clock,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(now: Clock) -> Self {
        Self {
            now,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `per_minute` is the sustained refill rate; `burst` is the bucket size.
    pub fn allow(&self, cls: &str, client: &str, per_minute: f64, burst: f64) -> bool {
        let key = format!("{cls}|{client}");
        let t = (self.now)();
        let mut map = self.buckets.lock();
        let b = map.entry(key).or_insert(Bucket {
            tokens: burst,
            at: t,
        });
        let elapsed = t.saturating_sub(b.at) as f64;
        b.tokens = burst.min(b.tokens + elapsed / 60_000.0 * per_minute);
        b.at = t;
        let ok = b.tokens >= 1.0;
        if ok {
            b.tokens -= 1.0;
        }
        ok
    }

    /// Drop buckets idle for 10 minutes so memory stays bounded.
    pub fn sweep(&self) {
        let cutoff = (self.now)().saturating_sub(10 * 60_000);
        self.buckets.lock().retain(|_, b| b.at >= cutoff);
    }

    pub fn len(&self) -> usize {
        self.buckets.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        let start = Instant::now();
        Self::new(Arc::new(move || start.elapsed().as_millis() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fake() -> (RateLimiter, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(0));
        let t2 = t.clone();
        (
            RateLimiter::new(Arc::new(move || t2.load(Ordering::SeqCst))),
            t,
        )
    }

    #[test]
    fn sixty_allowed_then_denied_then_refills() {
        let (l, t) = fake();
        for _ in 0..60 {
            assert!(l.allow("dl", "1.1.1.1", 60.0, 60.0));
        }
        assert!(!l.allow("dl", "1.1.1.1", 60.0, 60.0));
        assert!(l.allow("dl", "2.2.2.2", 60.0, 60.0));
        t.store(1000, Ordering::SeqCst);
        assert!(l.allow("dl", "1.1.1.1", 60.0, 60.0));
        assert!(!l.allow("dl", "1.1.1.1", 60.0, 60.0));
    }

    #[test]
    fn report_bucket_allows_exactly_five() {
        let (l, _t) = fake();
        let n = (0..10)
            .filter(|_| l.allow("report", "ip", 5.0 / 60.0, 5.0))
            .count();
        assert_eq!(n, 5);
    }

    #[test]
    fn sweep_drops_idle_buckets() {
        let (l, t) = fake();
        l.allow("a", "x", 10.0, 10.0);
        t.store(11 * 60_000, Ordering::SeqCst);
        l.allow("a", "y", 10.0, 10.0);
        l.sweep();
        assert_eq!(l.len(), 1);
    }
}
