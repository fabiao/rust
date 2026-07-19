//! Environment handling for ask.
//!
//! The current binary capability block carries no environment entries.
//! Report an empty environment without panicking; mutation remains
//! unsupported until the startup contract defines process-local storage.

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;

pub fn env() -> Env {
    Env::new(Vec::new())
}

pub fn getenv(_key: &OsStr) -> Option<OsString> {
    None
}

pub unsafe fn setenv(_key: &OsStr, _value: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "ask environment is immutable"))
}

pub unsafe fn unsetenv(_key: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "ask environment is immutable"))
}
