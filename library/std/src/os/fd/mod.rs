//! Owned and borrowed Unix-like file descriptors.
//!
//! This module is supported on Unix platforms and WASI, which both use a
//! similar file descriptor system for referencing OS resources.

#![stable(feature = "os_fd", since = "1.66.0")]
#![deny(unsafe_op_in_unsafe_fn)]

// `RawFd`, `AsRawFd`, etc.
#[cfg_attr(target_os = "ask", path = "raw_ask.rs")]
mod raw;

// `OwnedFd`, `AsFd`, etc.
#[cfg_attr(target_os = "ask", path = "owned_ask.rs")]
mod owned;

// Implementations for `AsRawFd` etc. for network types.
#[cfg(not(any(target_os = "trusty", target_os = "ask")))]
mod net;

// Implementation of stdio file descriptor constants.
#[cfg(not(target_os = "ask"))]
mod stdio;

#[cfg(test)]
#[cfg(not(target_os = "l4re"))]
mod tests;

// Export the types and traits for the public API.
#[stable(feature = "os_fd", since = "1.66.0")]
pub use owned::*;
#[stable(feature = "os_fd", since = "1.66.0")]
pub use raw::*;
#[unstable(feature = "stdio_fd_consts", issue = "150836")]
#[cfg(not(target_os = "ask"))]
pub use stdio::*;
