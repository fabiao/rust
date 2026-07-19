use crate::cell::Cell;

// ask: single-threaded processes (no thread-spawn syscall) but not a
// `singlethread` target - core-level atomics must stay real for its
// cross-process shared-memory rings (ask docs/10-rust-toolchain.md).
#[cfg(all(target_has_threads, not(target_os = "ask")))]
compile_error!("Using no_threads implementation on a target with threads");

pub struct Mutex {
    // This platform has no threads, so we can use a Cell here.
    locked: Cell<bool>,
}

unsafe impl Send for Mutex {}
unsafe impl Sync for Mutex {} // no threads on this platform

impl Mutex {
    #[inline]
    pub const fn new() -> Mutex {
        Mutex { locked: Cell::new(false) }
    }

    #[inline]
    pub fn lock(&self) {
        assert_eq!(self.locked.replace(true), false, "cannot recursively acquire mutex");
    }

    #[inline]
    pub unsafe fn unlock(&self) {
        self.locked.set(false);
    }

    #[inline]
    pub fn try_lock(&self) -> bool {
        !self.locked.replace(true)
    }
}
