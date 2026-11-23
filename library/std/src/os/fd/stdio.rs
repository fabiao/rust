#[cfg(target_os = "hermit")]
use hermit_abi as libc;
#[cfg(target_os = "motor")]
use moto_rt::libc;

use super::{AsFd, AsRawFd, BorrowedFd, RawFd};
use crate::io;

/// The file descriptor for the standard input stream of the current process.
///
/// See [`io::stdin()`][`crate::io::stdin`] for the higher level handle, which should be preferred
/// whenever possible. See [`STDERR`] for why the file descriptor might be required and caveats.
#[unstable(feature = "stdio_fd_consts", issue = "150836")]
pub const STDIN: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(0) };

/// The file descriptor for the standard output stream of the current process.
///
/// See [`io::stdout()`][`crate::io::stdout`] for the higher level handle, which should be preferred
/// whenever possible. See [`STDERR`] for why the file descriptor might be required and caveats. In
/// addition to the issues discussed there, note that [`Stdout`][`crate::io::Stdout`] is buffered by
/// default, and writing to the file descriptor will bypass this buffer.
#[unstable(feature = "stdio_fd_consts", issue = "150836")]
pub const STDOUT: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(1) };

/// The file descriptor for the standard error stream of the current process.
///
/// See [`io::stderr()`][`crate::io::stderr`] for the higher level handle, which should be preferred
/// whenever possible. However, there are situations where touching the `std::io` handles (or most
/// other parts of the standard library) risks deadlocks or other subtle bugs. For example:
///
/// - Global allocators must be careful to [avoid reentrancy][global-alloc-reentrancy], and the
///   `std::io` handles may allocate memory on (some) accesses.
/// - Signal handlers must be *async-signal-safe*, which rules out panicking, taking locks (may
///   deadlock if the signal handler interrupted a thread holding that lock), allocating memory, or
///   anything else that is not explicitly declared async-signal-safe.
/// - `CommandExt::pre_exec` callbacks can safely panic (with some limitations), but otherwise must
///   abide by similar limitations as signal handlers. In particular, at the time these callbacks
///   run, the stdio file descriptors have already been replaced, but the locks protecting the
///   `std::io` handles may be permanently locked if another thread held the lock at `fork()` time.
///
/// In these and similar cases, direct access to the file descriptor may be required. However, in
/// most cases, using the `std::io` handles and accessing the file descriptor via the `AsFd`
/// implementations is preferable, as it enables cooperation with the standard library's locking and
/// buffering.
///
/// # I/O safety
///
/// This is a `BorrowedFd<'static>` because the standard input/output/error streams are shared
/// resources that must remain available for the lifetime of the process. This is only true when
/// linking `std`, and may not always hold for [code running before `main()`][before-after-main] or
/// in `no_std` environments. It is [unsound][io-safety] to close these file descriptors. Safe
/// patterns for changing these file descriptors are available on Unix via the `StdioExt` extension
/// trait.
///
/// [before-after-main]: ../../../std/index.html#use-before-and-after-main
/// [io-safety]: ../../../std/io/index.html#io-safety
/// [global-alloc-reentrancy]: ../../../std/alloc/trait.GlobalAlloc.html#re-entrance
#[unstable(feature = "stdio_fd_consts", issue = "150836")]
pub const STDERR: BorrowedFd<'static> = unsafe { BorrowedFd::borrow_raw(2) };

#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for io::Stdin {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(0) }
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl<'a> AsFd for io::StdinLock<'a> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: user code should not close stdin out from under the standard library
        unsafe { BorrowedFd::borrow_raw(0) }
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for io::Stdout {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(1) }
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl<'a> AsFd for io::StdoutLock<'a> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: user code should not close stdout out from under the standard library
        unsafe { BorrowedFd::borrow_raw(1) }
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for io::Stderr {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(2) }
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl<'a> AsFd for io::StderrLock<'a> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: user code should not close stderr out from under the standard library
        unsafe { BorrowedFd::borrow_raw(2) }
    }
}

#[stable(feature = "asraw_stdio", since = "1.21.0")]
#[cfg(not(target_os = "trusty"))]
impl AsRawFd for io::Stdin {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDIN_FILENO
    }
}

#[stable(feature = "asraw_stdio", since = "1.21.0")]
impl AsRawFd for io::Stdout {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDOUT_FILENO
    }
}

#[stable(feature = "asraw_stdio", since = "1.21.0")]
impl AsRawFd for io::Stderr {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDERR_FILENO
    }
}

#[stable(feature = "asraw_stdio_locks", since = "1.35.0")]
#[cfg(not(target_os = "trusty"))]
impl<'a> AsRawFd for io::StdinLock<'a> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDIN_FILENO
    }
}

#[stable(feature = "asraw_stdio_locks", since = "1.35.0")]
impl<'a> AsRawFd for io::StdoutLock<'a> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDOUT_FILENO
    }
}

#[stable(feature = "asraw_stdio_locks", since = "1.35.0")]
impl<'a> AsRawFd for io::StderrLock<'a> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        libc::STDERR_FILENO
    }
}
