//! Single-process scheduling operations for ask.
//!
//! Full `std::thread` support waits for the Scheduler Server, but cooperative
//! yield and deadline-bounded sleep already have direct ABI primitives.

use crate::time::{Duration, Instant};

pub fn yield_now() {
    ask_abi::yield_now();
}

pub fn sleep(duration: Duration) {
    let Some(deadline) = Instant::now().checked_add(duration) else {
        // An unrepresentable deadline is effectively forever. Park in the
        // largest supported chunks, still permitting explicit wakes.
        loop {
            ask_abi::park_timeout(u64::MAX);
        }
    };

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        let millis = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(u128::from(u64::MAX)) as u64;
        ask_abi::park_timeout(millis.max(1));
    }
}
