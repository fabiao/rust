use crate::sys::sync::Mutex;
use crate::thread::sleep;
use crate::time::Duration;

// ask: single-threaded processes (no thread-spawn syscall) but not a
// `singlethread` target - core-level atomics must stay real for its
// cross-process shared-memory rings (ask docs/10-rust-toolchain.md).
#[cfg(all(target_has_threads, not(target_os = "ask")))]
compile_error!("Using no_threads implementation on a target with threads");

pub struct Condvar {}

impl Condvar {
    #[inline]
    pub const fn new() -> Condvar {
        Condvar {}
    }

    #[inline]
    pub fn notify_one(&self) {}

    #[inline]
    pub fn notify_all(&self) {}

    pub unsafe fn wait(&self, _mutex: &Mutex) {
        panic!("condvar wait not supported")
    }

    pub unsafe fn wait_timeout(&self, _mutex: &Mutex, dur: Duration) -> bool {
        sleep(dur);
        false
    }
}
