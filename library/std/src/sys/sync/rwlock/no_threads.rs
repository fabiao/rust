use crate::cell::Cell;

// ask: single-threaded processes (no thread-spawn syscall) but not a
// `singlethread` target - core-level atomics must stay real for its
// cross-process shared-memory rings (ask docs/10-rust-toolchain.md).
#[cfg(all(target_has_threads, not(target_os = "ask")))]
compile_error!("Using no_threads implementation on a target with threads");

pub struct RwLock {
    // This platform has no threads, so we can use a Cell here.
    mode: Cell<isize>,
}

unsafe impl Send for RwLock {}
unsafe impl Sync for RwLock {} // no threads on this platform

impl RwLock {
    #[inline]
    pub const fn new() -> RwLock {
        RwLock { mode: Cell::new(0) }
    }

    #[inline]
    pub fn read(&self) {
        let m = self.mode.get();
        if m >= 0 {
            self.mode.set(m.checked_add(1).expect("rwlock overflowed read locks"));
        } else {
            rtabort!("rwlock locked for writing");
        }
    }

    #[inline]
    pub fn try_read(&self) -> bool {
        let m = self.mode.get();
        if m >= 0 {
            if m == isize::MAX {
                return false;
            }
            self.mode.set(m + 1);
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn write(&self) {
        if self.mode.get() == 0 {
            self.mode.set(-1);
        } else {
            rtabort!("rwlock locked for reading");
        }
    }

    #[inline]
    pub fn try_write(&self) -> bool {
        if self.mode.get() == 0 {
            self.mode.set(-1);
            true
        } else {
            false
        }
    }

    #[inline]
    pub unsafe fn read_unlock(&self) {
        assert!(
            self.mode.replace(self.mode.get() - 1) > 0,
            "rwlock has not been locked for reading"
        );
    }

    #[inline]
    pub unsafe fn write_unlock(&self) {
        assert_eq!(self.mode.replace(0), -1, "rwlock has not been locked for writing");
    }

    #[inline]
    pub unsafe fn downgrade(&self) {
        assert_eq!(self.mode.replace(1), -1, "rwlock has not been locked for writing");
    }
}
