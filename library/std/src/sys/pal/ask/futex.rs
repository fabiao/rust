//! Process-private futex support for Rust synchronization primitives.

use crate::sync::atomic::Atomic;
use crate::time::Duration;

pub type Futex = Atomic<Primitive>;
pub type Primitive = u32;
pub type SmallFutex = Atomic<SmallPrimitive>;
pub type SmallPrimitive = u32;

pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let timeout = timeout.map_or(askrust::thread::FOREVER, |duration| {
        u64::try_from(duration.as_nanos()).unwrap_or(askrust::thread::FOREVER)
    });
    askrust::thread::futex_wait(futex, expected, timeout)
}

#[inline]
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    askrust::thread::futex_wake(futex)
}

#[inline]
pub fn futex_wake_all(futex: &Atomic<u32>) {
    askrust::thread::futex_wake_all(futex)
}
