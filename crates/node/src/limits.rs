//! What this agent signs at most, whatever the platform asks for.

use gtron_tron_address::TronAddress;
use serde::{Deserialize, Serialize};

use crate::job::JobKind;

pub const DAY_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone)]
pub struct LocalLimits {
    pub max_balance_sun: i64,
    pub max_lock_blocks: i64,
    /// Empty = any receiver (only with NODE_ALLOW_ANY_RECEIVER).
    pub allowed_receivers: Vec<TronAddress>,
    pub max_daily_sun: i64,
    pub max_locked_sun: i64,
}

impl LocalLimits {
    pub fn check(
        &self,
        kind: JobKind,
        receiver: &TronAddress,
        balance_sun: i64,
        lock_blocks: Option<i64>,
    ) -> Result<(), String> {
        if balance_sun <= 0 || balance_sun > self.max_balance_sun {
            return Err(format!(
                "balance {balance_sun} SUN is outside this agent's limit of {}",
                self.max_balance_sun
            ));
        }
        if let Some(b) = lock_blocks {
            if b <= 0 || b > self.max_lock_blocks {
                return Err(format!(
                    "lock of {b} blocks is outside this agent's limit of {}",
                    self.max_lock_blocks
                ));
            }
        }
        // Reclaiming is always allowed: it only returns our own stake.
        if kind == JobKind::Delegate
            && !self.allowed_receivers.is_empty()
            && !self.allowed_receivers.contains(receiver)
        {
            return Err(format!(
                "receiver {receiver} is not on this agent's allowlist"
            ));
        }
        Ok(())
    }

    /// The rolling caps over what was already signed (`events`), with `ev` added.
    pub fn check_caps(&self, events: &[CapEvent], ev: &CapEvent, now: i64) -> Result<(), String> {
        let daily: i64 = events
            .iter()
            .filter(|e| e.at_ms > now - DAY_MS)
            .map(|e| e.balance_sun)
            .sum::<i64>()
            .saturating_add(ev.balance_sun);
        if daily > self.max_daily_sun {
            return Err(format!(
                "24h delegated stake would be {daily} SUN, over this agent's limit of {}",
                self.max_daily_sun
            ));
        }
        if ev.locked_until_ms > now {
            let locked: i64 = events
                .iter()
                .filter(|e| e.locked_until_ms > now)
                .map(|e| e.balance_sun)
                .sum::<i64>()
                .saturating_add(ev.balance_sun);
            if locked > self.max_locked_sun {
                return Err(format!(
                    "locked stake would be {locked} SUN, over this agent's limit of {}",
                    self.max_locked_sun
                ));
            }
        }
        Ok(())
    }
}

/// Add `ev` to `events` and drop the ones that no longer count.
///
/// On chain a wallet has one locked delegation per receiver: a new locked delegation to the
/// same receiver adds to it and moves the expiry of the whole amount to the new one (an
/// already expired lock is released first). Still-locked events for that receiver get the
/// new expiry here too, or the locked cap would let their stake go again while it is in fact
/// still locked. Events recorded without a receiver are treated as the same receiver.
pub fn record(events: &mut Vec<CapEvent>, ev: CapEvent, now: i64) {
    if ev.locked_until_ms > now {
        for e in events.iter_mut() {
            let same = match (&e.receiver, &ev.receiver) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            if same && e.locked_until_ms > now {
                e.locked_until_ms = e.locked_until_ms.max(ev.locked_until_ms);
            }
        }
    }
    events.retain(|e| e.at_ms > now - DAY_MS || e.locked_until_ms > now);
    events.push(ev);
}

/// One signed delegation, for the 24h and still-locked caps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapEvent {
    pub job: String,
    pub at_ms: i64,
    pub balance_sun: i64,
    pub locked_until_ms: i64,
    /// Absent in records written by gtron-node 0.1.0 and earlier.
    #[serde(default)]
    pub receiver: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const CLIENT: &str = "TEMkRxLtCCdL4BCwbPXbbNWe4a9gtJ7kq7";

    pub fn limits() -> LocalLimits {
        LocalLimits {
            max_balance_sun: 1_000_000_000,
            max_lock_blocks: 28_800,
            allowed_receivers: vec![],
            max_daily_sun: 3_000_000_000,
            max_locked_sun: 2_000_000_000,
        }
    }

    #[test]
    fn local_limits() {
        let ok: TronAddress = CLIENT.parse().unwrap();
        let other: TronAddress = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".parse().unwrap();
        let l = LocalLimits {
            allowed_receivers: vec![ok.clone()],
            ..limits()
        };
        let d = JobKind::Delegate;
        assert!(l.check(d, &ok, 1_000_000_000, Some(1200)).is_ok());
        assert!(l.check(d, &ok, 1_000_000_001, Some(1200)).is_err());
        assert!(l.check(d, &ok, 0, None).is_err());
        assert!(l.check(d, &ok, 1, Some(28_801)).is_err());
        assert!(l.check(d, &other, 1, None).is_err());
        assert!(l.check(JobKind::Undelegate, &other, 1, None).is_ok());
    }

    #[test]
    fn rolling_caps() {
        let l = limits();
        let now = 10 * DAY_MS;
        let ev = |at_ms, balance_sun, locked_until_ms| CapEvent {
            job: "j".into(),
            at_ms,
            balance_sun,
            locked_until_ms,
            receiver: Some(CLIENT.into()),
        };
        // Two locked delegations today: the locked cap (2 000 TRX) is reached first.
        let events = vec![ev(now - 1000, 1_000_000_000, now + DAY_MS)];
        assert!(l
            .check_caps(&events, &ev(now, 1_000_000_000, now + DAY_MS), now)
            .is_ok());
        let events = vec![
            ev(now - 2000, 1_000_000_000, now + DAY_MS),
            ev(now - 1000, 1_000_000_000, now + DAY_MS),
        ];
        assert!(l.check_caps(&events, &ev(now, 1, now + 10), now).is_err());
        // Unlocked: only the daily cap applies.
        assert!(l
            .check_caps(&events, &ev(now, 1_000_000_000, now), now)
            .is_ok());
        assert!(l
            .check_caps(&events, &ev(now, 1_000_000_001, now), now)
            .is_err());
        // Older than 24h and unlocked: no longer counts.
        let old = vec![ev(now - DAY_MS - 1, 3_000_000_000, now - 1)];
        assert!(l
            .check_caps(&old, &ev(now, 1_000_000_000, now + 10), now)
            .is_ok());
    }

    #[test]
    fn a_new_lock_extends_the_older_one_to_the_same_receiver() {
        let l = limits();
        let now = 10 * DAY_MS;
        let ev = |job: &str, at_ms, balance_sun, locked_until_ms, to: Option<&str>| CapEvent {
            job: job.into(),
            at_ms,
            balance_sun,
            locked_until_ms,
            receiver: to.map(Into::into),
        };
        let other = "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t";
        // 2 000 TRX locked to CLIENT until day 11, 1 TRX more on day 10.9 locked until day 12.
        let mut events = vec![ev(
            "a",
            now - DAY_MS,
            1_999_000_000,
            now + DAY_MS,
            Some(CLIENT),
        )];
        let small = ev("b", now, 1_000_000, now + 2 * DAY_MS, Some(CLIENT));
        assert!(l.check_caps(&events, &small, now).is_ok());
        record(&mut events, small, now);
        assert_eq!(events[0].locked_until_ms, now + 2 * DAY_MS);
        // On day 11.5 the first 2 000 TRX are still locked on chain: no room for more.
        let later = now + DAY_MS + DAY_MS / 2;
        let big = ev("c", later, 1_000_000_000, later + DAY_MS, Some(other));
        assert!(l.check_caps(&events, &big, later).is_err());
        // A lock to another receiver does not extend it.
        let mut events = vec![ev("a", now - DAY_MS, 1, now + DAY_MS, Some(CLIENT))];
        record(
            &mut events,
            ev("b", now, 1, now + 2 * DAY_MS, Some(other)),
            now,
        );
        assert_eq!(events[0].locked_until_ms, now + DAY_MS);
        // Unknown receiver (old record): assume it is the same one.
        let mut events = vec![ev("a", now - DAY_MS, 1, now + DAY_MS, None)];
        record(
            &mut events,
            ev("b", now, 1, now + 2 * DAY_MS, Some(other)),
            now,
        );
        assert_eq!(events[0].locked_until_ms, now + 2 * DAY_MS);
        // An expired lock is released first and stays expired.
        let mut events = vec![ev("a", now - 1000, 1, now - 1, Some(CLIENT))];
        record(
            &mut events,
            ev("b", now, 1, now + DAY_MS, Some(CLIENT)),
            now,
        );
        assert_eq!(events[0].locked_until_ms, now - 1);
    }
}
