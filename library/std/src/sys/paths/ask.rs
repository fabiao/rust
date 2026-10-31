//! The process working directory lives in the resident descriptor owner, so
//! Rust and C code in one process share it.

use crate::io;
use crate::path::{Path, PathBuf};

pub fn getcwd() -> io::Result<PathBuf> {
    let mut buffer = [0u8; ask_io::fs::PATH_MAX];
    let path = crate::sys::fs::working_directory(&mut buffer)?;
    Ok(PathBuf::from(path))
}

/// The view root holds only mount points and is always a valid directory;
/// any other target must be a reachable directory.
pub fn chdir(path: &Path) -> io::Result<()> {
    let mut buffer = [0u8; ask_io::fs::PATH_MAX];
    let target = crate::sys::fs::absolute_view_path(path, &mut buffer)?;
    if target != "/" && !crate::fs::metadata(target)?.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    crate::sys::fd::retry(|error| unsafe {
        ask_fd::__askfd_set_cwd(target.as_ptr(), target.len(), error)
    })
}
