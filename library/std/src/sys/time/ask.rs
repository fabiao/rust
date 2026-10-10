//! Monotonic and wall-clock time for ask.
//!
//! `Instant` uses the kernel's millisecond monotonic clock (`GetMonotonicMs`).
//! `SystemTime::now` reads the CMOS-RTC-backed wall clock (`GetWallTimeMs`,
//! `kernel/src/arch/x86_64/rtc.rs`): not monotonic, UTC milliseconds since
//! the Unix epoch.

use crate::time::Duration;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

/// Signed offset from the Unix epoch; file timestamps and parsed dates may
/// precede it. `nanos` stays below one second, so field order is time order.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime {
    secs: i64,
    nanos: u32,
}

pub const UNIX_EPOCH: SystemTime = SystemTime { secs: 0, nanos: 0 };

const NANOS_PER_SEC: u32 = 1_000_000_000;
const MILLIS_PER_SEC: u64 = 1_000;
const NANOS_PER_MILLI: u32 = 1_000_000;

impl Instant {
    pub fn now() -> Instant {
        Instant(Duration::from_millis(ask_sys::get_monotonic_ms()))
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }
}

impl SystemTime {
    pub const MAX: SystemTime = SystemTime { secs: i64::MAX, nanos: NANOS_PER_SEC - 1 };

    pub const MIN: SystemTime = SystemTime { secs: i64::MIN, nanos: 0 };

    pub fn now() -> SystemTime {
        let millis = ask_sys::get_wall_time_ms();
        SystemTime {
            secs: (millis / MILLIS_PER_SEC) as i64,
            nanos: (millis % MILLIS_PER_SEC) as u32 * NANOS_PER_MILLI,
        }
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        if self >= other { Ok(self.distance_from(other)) } else { Err(other.distance_from(self)) }
    }

    /// The span from an earlier `other`; any two `i64` second counts differ by
    /// at most `u64::MAX` seconds.
    fn distance_from(&self, other: &SystemTime) -> Duration {
        let mut secs = (self.secs as i128 - other.secs as i128) as u64;
        let nanos = if self.nanos >= other.nanos {
            self.nanos - other.nanos
        } else {
            secs -= 1;
            self.nanos + NANOS_PER_SEC - other.nanos
        };
        Duration::new(secs, nanos)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        let mut secs = self.secs as i128 + other.as_secs() as i128;
        let mut nanos = self.nanos + other.subsec_nanos();
        if nanos >= NANOS_PER_SEC {
            nanos -= NANOS_PER_SEC;
            secs += 1;
        }
        Some(SystemTime { secs: i64::try_from(secs).ok()?, nanos })
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        let mut secs = self.secs as i128 - other.as_secs() as i128;
        let nanos = if self.nanos >= other.subsec_nanos() {
            self.nanos - other.subsec_nanos()
        } else {
            secs -= 1;
            self.nanos + NANOS_PER_SEC - other.subsec_nanos()
        };
        Some(SystemTime { secs: i64::try_from(secs).ok()?, nanos })
    }
}
