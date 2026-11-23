//! System bindings for ask. `std` calls the ASK runtime only through `askrust`;
//! it does not depend on `askme` or `askposix`.
//! Everything not implemented in a `sys/*/ask.rs` file falls through to the
//! matching `sys/*/unsupported.rs` automatically.

#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(missing_docs, nonstandard_style)]

pub mod futex;

use crate::io;

#[cfg(not(test))]
pub(crate) use askrust::rt::start;

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::const_error!(io::ErrorKind::Unsupported, "operation not supported on ask yet")
}

pub fn abort_internal() -> ! {
    askrust::rt::abort()
}

/// A runtime error as an `io::Error` whose raw OS code is the runtime code.
pub(crate) fn ask_error(error: askrust::Error) -> io::Error {
    io::Error::from_raw_os_error(i32::from(error.code()))
}

// SAFETY: must be called only once during runtime initialization.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
// NOTE: this is not guaranteed to run, for example when the program aborts.
pub unsafe fn cleanup() {}

#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn __ask_std_start() -> ! {
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8, sigpipe: u8) -> i32;
    }
    // Compiler main reaches rt before language initialization; its entry hook
    // stages descriptors and awaits GO before invoking rt's initialize callback.
    // Thread-local access requires primitive TLS even before that hook.
    unsafe { askrust::thread::tls::initialize() };
    // `sys::args` and `sys::env` read argv and the environment from the
    // runtime; the CRT argc/argv pair stays empty, matching Motor OS's `motor_start`.
    let result = unsafe { main(0, core::ptr::null(), 0) };
    askrust::rt::exit(result)
}
