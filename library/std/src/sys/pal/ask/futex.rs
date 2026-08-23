//! Process-private futex support for Rust synchronization primitives.

use crate::sync::atomic::{Atomic, Ordering};
use crate::time::{Duration, Instant};

pub type Futex = Atomic<Primitive>;
pub type Primitive = u32;
pub type SmallFutex = Atomic<SmallPrimitive>;
pub type SmallPrimitive = u32;

pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let deadline = timeout.and_then(|duration| Instant::now().checked_add(duration));
    while futex.load(Ordering::Relaxed) == expected {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return false;
        }
        crate::thread::yield_now();
    }
    true
}

#[inline]
pub fn futex_wake(_futex: &Atomic<u32>) -> bool {
    true
}

#[inline]
pub fn futex_wake_all(_futex: &Atomic<u32>) {}
