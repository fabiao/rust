//! Process-local environment over the spawn-time `GetSpawnBlob` blob.
//!
//! The kernel's copy is immutable. Mutation here is PAL-local, matching
//! libc `environ`: `set_var` is visible to later `var`/`Command` inherit
//! and is not written back into the capability block.

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;
use crate::sync::{Mutex, OnceLock};
use crate::sys::args::decode_nul_fields;
use crate::sys::os_str::Buf;
use crate::sys::{AsInner, FromInner};

struct Store {
    pairs: Vec<(OsString, OsString)>,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_inner(Buf { inner: bytes.to_vec() })
}

fn os_bytes(value: &OsStr) -> &[u8] {
    &value.as_inner().inner
}

fn load() -> Store {
    let mut blob = [0u8; ask_abi::MAX_ARGV_ENV_LEN];
    let len = match ask_sys::get_env(&mut blob) {
        Ok(len) => len,
        Err(_) => return Store { pairs: Vec::new() },
    };
    let mut pairs = Vec::new();
    for entry in decode_nul_fields(blob.get(..len).unwrap_or(&[])) {
        let Some(split) = entry.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = entry.split_at(split);
        if key.is_empty() {
            continue;
        }
        let Some(value) = value.get(1..) else {
            continue;
        };
        pairs.push((os_from_bytes(key), os_from_bytes(value)));
    }
    Store { pairs }
}

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| Mutex::new(load()))
}

fn keys_equal(a: &OsStr, b: &OsStr) -> bool {
    os_bytes(a) == os_bytes(b)
}

pub fn env() -> Env {
    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    Env::new(guard.pairs.clone())
}

pub fn getenv(key: &OsStr) -> Option<OsString> {
    let guard = store().lock().unwrap_or_else(|e| e.into_inner());
    guard.pairs.iter().find(|(k, _)| keys_equal(k, key)).map(|(_, v)| v.clone())
}

pub unsafe fn setenv(key: &OsStr, value: &OsStr) -> io::Result<()> {
    if os_bytes(key).is_empty() || os_bytes(key).contains(&b'=') {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "environment variable key must be nonempty and must not contain '='"
        ));
    }
    let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = guard.pairs.iter_mut().find(|(k, _)| keys_equal(k, key)) {
        slot.1 = value.to_owned();
    } else {
        guard.pairs.push((key.to_owned(), value.to_owned()));
    }
    Ok(())
}

pub unsafe fn unsetenv(key: &OsStr) -> io::Result<()> {
    let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
    guard.pairs.retain(|(k, _)| !keys_equal(k, key));
    Ok(())
}
