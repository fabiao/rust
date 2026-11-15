//! Threads for ask over the runtime, which owns stacks and thread lifecycle.
//! `available_parallelism`/`current_os_id`/`set_name` stay on
//! `sys::thread::unsupported`.

use core::ffi::c_void;

use crate::io;
use crate::thread::ThreadInit;
use crate::time::Duration;

pub const DEFAULT_MIN_STACK_SIZE: usize = 256 * 1024;

pub struct Thread {
    handle: u64,
    joined: bool,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

unsafe extern "C" fn thread_start(init: *mut c_void) {
    crate::sys::thread_local::key::init_this_thread();
    // SAFETY: `Thread::new` leaked this box for exactly this thread.
    let init = unsafe { Box::from_raw(init.cast::<ThreadInit>()) };
    let rust_start = init.init();
    rust_start();
    unsafe { crate::sys::thread_local::key::destroy_tls() };
}

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let init = Box::into_raw(init);
        // SAFETY: `thread_start` takes ownership of `init` on the new thread.
        match unsafe { askrust::thread::spawn(stack, thread_start, init.cast()) } {
            Ok(handle) => Ok(Thread { handle, joined: false }),
            Err(error) => {
                drop(unsafe { Box::from_raw(init) });
                Err(crate::sys::pal::ask_error(error))
            }
        }
    }

    pub fn join(mut self) {
        #[cfg(ask_class1_verify)]
        panic!("Class 1 verification tripwire: std::thread::JoinHandle::join is prohibited");
        askrust::thread::join(self.handle).expect("failed to join ASK thread");
        self.joined = true;
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        if !self.joined {
            askrust::thread::detach(self.handle);
        }
    }
}

pub fn yield_now() {
    askrust::thread::yield_now();
}

pub fn sleep(duration: Duration) {
    #[cfg(ask_class1_verify)]
    panic!("Class 1 verification tripwire: std::thread::sleep is prohibited");
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(askrust::thread::FOREVER);
    askrust::thread::sleep_ns(nanos);
}
