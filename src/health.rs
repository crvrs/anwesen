//! Shared health state surfaced by the `/health` endpoint per [ANW-8].
//!
//! The fields the User Manual exposes are written from three places:
//!
//! - `in_flight_rescan` -- set by `vault_scanner` around each walk
//!   (startup or `RescanNow`);
//! - `last_index_update_ts` -- set by `index_writer` on each successful
//!   Rebuild / Batch / Upsert / Delete commit;
//! - `last_event_ts` and `watcher_state` -- set by `filesystem_watcher`
//!   as raw events arrive (or fail to bind, per [[ADR-003 Filesystem
//!   Change Tracking]]).
//!
//! Reads come from the HTTP layer. Atomic counters are used so updates
//! never contend on a lock; `chrono::DateTime<Utc>` is stored as a Unix
//! millisecond stamp (0 means "never").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};

use chrono::{DateTime, TimeZone, Utc};

/// Stable identifier for `watcher_state` -- the inotify binding only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    Running,
    Degraded,
}

impl WatcherState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Default)]
pub struct HealthState {
    in_flight_rescan: AtomicBool,
    last_index_update_ms: AtomicI64,
    last_event_ms: AtomicI64,
    watcher_state: AtomicU8, // 0 = Running, 1 = Degraded
}

impl HealthState {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_in_flight_rescan(&self, on: bool) {
        self.in_flight_rescan.store(on, Ordering::Release);
    }

    /// # Panics
    /// Never; the timestamp is unconditionally stored.
    pub fn record_index_update(&self, at: DateTime<Utc>) {
        self.last_index_update_ms
            .store(at.timestamp_millis(), Ordering::Release);
    }

    pub fn record_event(&self, at: DateTime<Utc>) {
        self.last_event_ms
            .store(at.timestamp_millis(), Ordering::Release);
    }

    pub fn set_watcher_state(&self, s: WatcherState) {
        self.watcher_state.store(
            match s {
                WatcherState::Running => 0,
                WatcherState::Degraded => 1,
            },
            Ordering::Release,
        );
    }

    #[must_use]
    pub fn in_flight_rescan(&self) -> bool {
        self.in_flight_rescan.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn last_index_update(&self) -> Option<DateTime<Utc>> {
        let ms = self.last_index_update_ms.load(Ordering::Acquire);
        if ms == 0 {
            None
        } else {
            Utc.timestamp_millis_opt(ms).single()
        }
    }

    #[must_use]
    pub fn last_event(&self) -> Option<DateTime<Utc>> {
        let ms = self.last_event_ms.load(Ordering::Acquire);
        if ms == 0 {
            None
        } else {
            Utc.timestamp_millis_opt(ms).single()
        }
    }

    #[must_use]
    pub fn watcher_state(&self) -> WatcherState {
        if self.watcher_state.load(Ordering::Acquire) == 0 {
            WatcherState::Running
        } else {
            WatcherState::Degraded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_reads_initial_values() {
        let s = HealthState::default();
        assert!(!s.in_flight_rescan());
        assert!(s.last_index_update().is_none());
        assert!(s.last_event().is_none());
        assert_eq!(s.watcher_state(), WatcherState::Running);
    }

    #[test]
    fn rescan_flag_round_trips() {
        let s = HealthState::default();
        s.set_in_flight_rescan(true);
        assert!(s.in_flight_rescan());
        s.set_in_flight_rescan(false);
        assert!(!s.in_flight_rescan());
    }

    #[test]
    fn timestamp_round_trips_at_millisecond_resolution() {
        let s = HealthState::default();
        let now = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        s.record_index_update(now);
        // Stored as ms; expect the same wall-clock value back.
        assert_eq!(s.last_index_update(), Some(now));
    }

    #[test]
    fn watcher_state_round_trips() {
        let s = HealthState::default();
        s.set_watcher_state(WatcherState::Degraded);
        assert_eq!(s.watcher_state(), WatcherState::Degraded);
        s.set_watcher_state(WatcherState::Running);
        assert_eq!(s.watcher_state(), WatcherState::Running);
    }
}
