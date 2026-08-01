//! System bindings for ask (docs/10-rust-toolchain.md, docs/08's mixed-
//! binaries decision: std binds `ask-abi` directly, never `askposix`).
//! Everything not implemented in a `sys/*/ask.rs` file falls through to the
//! matching `sys/*/unsupported.rs` automatically — no `target_os = "ask"`
//! arm is needed in those selectors for that to happen.

#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(missing_docs, nonstandard_style)]

pub mod channel;

use crate::io;

/// `ask_abi::Error` carries no errno-shaped detail yet (docs/02's open
/// syscall-enumeration question) — every failure reports the same generic
/// OS-error code until that's designed.
pub(crate) fn map_ask_error(_err: ask_abi::Error) -> io::Error {
    io::Error::from_raw_os_error(1)
}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::const_error!(io::ErrorKind::Unsupported, "operation not supported on ask yet")
}

pub fn abort_internal() -> ! {
    ask_abi::exit(u64::MAX)
}

// SAFETY: must be called only once during runtime initialization.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
// NOTE: this is not guaranteed to run, for example when the program aborts.
pub unsafe fn cleanup() {}

#[cfg(not(test))]
#[unsafe(no_mangle)]
extern "sysv64" fn _start() -> ! {
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8, sigpipe: u8) -> i32;
    }
    // `SpawnRaw`'s binary capability block carries no argument list
    // (docs/02-kernel-abi.md) — every ask process starts with an empty
    // argv, the same posture Motor OS's `motor_start` takes.
    let result = unsafe { main(0, core::ptr::null(), 0) };
    ask_abi::exit(result as u32 as u64)
}
