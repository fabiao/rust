//! Startup arguments from `GetSpawnBlob(SPAWN_BLOB_ARGV)`.
//!
//! The kernel copies an opaque NUL-separated blob and never parses it.
//! Fields are separated, not terminated; a trailing empty field from an
//! optional final NUL is dropped so producers that emit either form agree.

pub use super::common::Args;
use crate::ffi::OsString;
use crate::sync::OnceLock;
use crate::sys::os_str::Buf;
use crate::sys::FromInner;

static ARGS: OnceLock<Vec<OsString>> = OnceLock::new();

fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_inner(Buf { inner: bytes.to_vec() })
}

fn load() -> Vec<OsString> {
    let mut blob = [0u8; ask_abi::MAX_ARGV_ENV_LEN];
    let len = match ask_sys::get_argv(&mut blob) {
        Ok(len) => len,
        Err(_) => return Vec::new(),
    };
    decode_nul_fields(blob.get(..len).unwrap_or(&[])).map(os_from_bytes).collect()
}

pub(crate) fn decode_nul_fields(blob: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut fields: Vec<&[u8]> = if blob.is_empty() {
        Vec::new()
    } else {
        blob.split(|&b| b == 0).collect()
    };
    if fields.last() == Some(&&[][..]) {
        fields.pop();
    }
    fields.into_iter()
}

pub fn args() -> Args {
    let loaded = ARGS.get_or_init(load);
    Args::new(loaded.clone())
}
