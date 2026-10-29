//! Exponential backoff for failed polls and for jobs that are signed but not finished.
//!
//! Without it a job stuck in `Retry` (TRON node down, platform unreachable) comes back on
//! every poll at once, and the agent hammers both: TronGrid rate-limits and then bans the
//! shop's API key, which takes the shop off the market.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    attempts: u32,
}

impl Backoff {
    pub const fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempts: 0,
        }
    }

    /// The wait before the next attempt: base · 2^n, capped, ±20% so that many agents
    /// coming back after a platform outage do not all retry in the same second.
    pub fn next_delay(&mut self) -> Duration {
        let exp = self.base.saturating_mul(1 << self.attempts.min(16));
        self.attempts = self.attempts.saturating_add(1);
        jitter(exp.min(self.max))
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

fn jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    // 0.8 ..= 1.2 in thousandths; time-seeded is enough to spread agents apart.
    let permille = 800 + u64::from(nanos % 401);
    let ms = d.as_millis().min(u128::from(u64::MAX)) as u64;
    Duration::from_millis(ms.saturating_mul(permille) / 1000)
}

struct Entry {
    backoff: Backoff,
    due: Instant,
}

/// Per-job backoff: which jobs are waiting out a failure, and until when.
pub struct RetryQueue {
    base: Duration,
    max: Duration,
    jobs: HashMap<String, Entry>,
}

impl RetryQueue {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            jobs: HashMap::new(),
        }
    }

    pub fn is_due(&self, job: &str, now: Instant) -> bool {
        self.jobs.get(job).is_none_or(|e| e.due <= now)
    }

    /// Record a failed attempt; returns the delay and how many attempts failed so far.
    pub fn failed(&mut self, job: &str, now: Instant) -> (Duration, u32) {
        let e = self.jobs.entry(job.to_owned()).or_insert(Entry {
            backoff: Backoff::new(self.base, self.max),
            due: now,
        });
        let d = e.backoff.next_delay();
        e.due = now + d;
        (d, e.backoff.attempts())
    }

    /// A job finished or given back: leave it alone until `until`. The platform closes it
    /// at once, but if a lagging list shows it again, it must not loop straight back in.
    pub fn hold(&mut self, job: &str, until: Instant) {
        self.jobs.insert(
            job.to_owned(),
            Entry {
                backoff: Backoff::new(self.base, self.max),
                due: until,
            },
        );
    }

    /// Forget jobs the platform no longer lists as open.
    pub fn retain_open(&mut self, open: &HashSet<&str>) {
        self.jobs.retain(|id, _| open.contains(id.as_str()));
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps() {
        let mut b = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        let d: Vec<u64> = (0..8).map(|_| b.next_delay().as_millis() as u64).collect();
        let within = |ms: u64, target: u64| ms >= target * 8 / 10 && ms <= target * 12 / 10;
        assert!(within(d[0], 2_000), "{d:?}");
        assert!(within(d[1], 4_000), "{d:?}");
        assert!(within(d[2], 8_000), "{d:?}");
        assert!(d[5..].iter().all(|&ms| within(ms, 60_000)), "{d:?}");
        b.reset();
        assert!(within(b.next_delay().as_millis() as u64, 2_000));
    }

    #[test]
    fn huge_attempt_counts_do_not_overflow() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        for _ in 0..1000 {
            assert!(b.next_delay() <= Duration::from_secs(72));
        }
    }

    #[test]
    fn queue_holds_failed_jobs_until_due() {
        let now = Instant::now();
        let mut q = RetryQueue::new(Duration::from_secs(2), Duration::from_secs(60));
        assert!(q.is_due("a", now));
        let (d, n) = q.failed("a", now);
        assert_eq!(n, 1);
        assert!(!q.is_due("a", now));
        assert!(q.is_due("a", now + d));
        let (d2, n) = q.failed("a", now + d);
        assert_eq!(n, 2);
        assert!(d2 > d);
        q.retain_open(&HashSet::from(["b"]));
        assert_eq!(q.len(), 0, "closed jobs are forgotten");
        q.failed("b", now);
        q.failed("b", now);
        q.hold("b", now + Duration::from_secs(30));
        assert!(!q.is_due("b", now + Duration::from_secs(29)));
        let (d, n) = q.failed("b", now + Duration::from_secs(30));
        assert_eq!(n, 1, "a hold starts the backoff over");
        assert!(d <= Duration::from_millis(2400));
    }
}
