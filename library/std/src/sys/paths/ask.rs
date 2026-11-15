//! The process working directory lives in the resident descriptor owner, so
//! Rust and C code in one process share it.

use crate::io;
use crate::path::{Path, PathBuf};

pub fn getcwd() -> io::Result<PathBuf> {
    crate::sys::fs::read_string(askrust::fs::cwd).map(PathBuf::from)
}

pub fn temp_dir() -> PathBuf {
    crate::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(
            crate::sys::fs::read_string(|buffer| Ok(askrust::fs::temp_dir(buffer)))
                .unwrap_or_default(),
        )
    })
}

pub fn chdir(path: &Path) -> io::Result<()> {
    let path = path.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))?;
    askrust::fs::set_cwd(path).map_err(crate::sys::pal::ask_error)
}
