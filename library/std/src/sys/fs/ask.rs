//! `std::fs` PAL for ask: a blocking `File` bridging onto `askfs`'s
//! `FS_OP_*` wire protocol (`ask_io::fs`) over a `SyncChannel`
//! (`sys::pal::ask::channel`, re-exported as `sys::channel`). One fresh
//! channel per `File::open` — `askfs`
//! itself allows only one open handle per accepted session
//! (`MAX_OPEN_HANDLES = 1`), so multiplexing several files over one channel
//! isn't possible even in principle.
//!
//! The process's namespace bindings arrive once at spawn time in the
//! fixed startup view (`GetSpawnBlob(SPAWN_BLOB_VIEW,..)`,
//! `ask_abi::view`); this module decodes mount capability tokens and optional
//! source roots so paths like `/out/marker.txt` or `/in/asset.txt` map to
//! provider-relative opens (`profiles/…/out/marker.txt`, `apptest/in/asset.txt`).

use crate::ffi::OsString;
use crate::hash::Hash;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
use crate::sync::Mutex;
pub use crate::sys::fs::common::Dir;
use crate::sys::channel::SyncChannel;
use crate::sys::pal::unsupported_err;
use crate::sys::time::SystemTime;
use crate::sys::unsupported;

fn startup_view() -> io::Result<ask_io::view::View> {
    let mut bytes = [0u8; ask_abi::view::LEN];
    ask_sys::get_startup_view(&mut bytes).map_err(crate::sys::map_ask_error)?;
    ask_io::view::View::decode_startup(&bytes).map_err(|_| unsupported_err())
}

/// Map a bound process path to `(provider_token, provider-relative path bytes)`.
fn resolve_fs_path(path: &Path) -> io::Result<(u32, heapless_path::PathBuf)> {
    let path = path.to_str().ok_or_else(unsupported_err)?;
    let view = startup_view()?;
    let resolved = view
        .resolve(path, ask_io::view::Operation::Read)
        .map_err(|_| unsupported_err())?;
    let mut encoded = [0u8; ask_io::fs::OPEN_PATH_MAX];
    let relative = resolved
        .encode_path(&mut encoded)
        .map_err(|_| unsupported_err())?;
    let mut out = heapless_path::PathBuf::new();
    out.push(relative.as_bytes())?;
    Ok((resolved.provider_token, out))
}

/// Tiny fixed path builder — avoids `alloc` in the hot open path while still
/// fitting `ask_io::fs::OPEN_PATH_MAX`.
mod heapless_path {
    use super::*;

    pub struct PathBuf {
        buf: [u8; ask_io::fs::OPEN_PATH_MAX],
        len: usize,
    }

    impl PathBuf {
        pub fn new() -> Self {
            Self {
                buf: [0; ask_io::fs::OPEN_PATH_MAX],
                len: 0,
            }
        }

        pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
            let end = self
                .len
                .checked_add(bytes.len())
                .ok_or_else(unsupported_err)?;
            if end > ask_io::fs::OPEN_PATH_MAX {
                return Err(unsupported_err());
            }
            self.buf[self.len..end].copy_from_slice(bytes);
            self.len = end;
            Ok(())
        }

        pub fn as_bytes(&self) -> &[u8] {
            &self.buf[..self.len]
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePermissions {
    readonly: bool,
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileType {
    is_dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !self.is_dir
    }

    /// Symlink targets are currently resolved provider-side before this PAL.
    pub fn is_symlink(&self) -> bool {
        false
    }
}

/// Path-addressed metadata, or the size-only projection an open `File` can
/// answer from `FS_OP_OPEN`. `askfs` carries no creation
/// timestamp, so `created()` stays unsupported while `modified()`/`accessed()`
/// resolve whenever the attributes came from `stat` (docs/vfs-layout.md).
#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    mode: u32,
    is_dir: bool,
    /// `None` for the open-handle projection, which has no timestamps on the
    /// wire; `Some` once a metadata query supplied them.
    times: Option<FileStatTimes>,
}

#[derive(Copy, Clone)]
struct FileStatTimes {
    accessed_sec: i64,
    modified_sec: i64,
}

/// The native metadata mode's owner-write bit.
/// server-side, so readonly is decided by the same bit `FS_OP_FCHMOD` sets.
const MODE_OWNER_WRITE: u32 = 0o200;

/// Permission bits requested for a `DirBuilder`-created directory: owner
/// read/write/execute plus group and other read/execute, matching the POSIX
/// `0777 & ~umask` default shells expect. `askfs` applies its own mask.
const DEFAULT_DIR_MODE: u32 = 0o755;

/// Native timestamps are signed nanoseconds from the Unix epoch; `SystemTime`
/// is an unsigned offset from it, so a pre-epoch stamp subtracts instead.
fn system_time_from_secs(secs: i64) -> SystemTime {
    let magnitude = crate::time::Duration::from_secs(secs.unsigned_abs());
    let shifted = if secs < 0 {
        crate::sys::time::UNIX_EPOCH.checked_sub_duration(&magnitude)
    } else {
        crate::sys::time::UNIX_EPOCH.checked_add_duration(&magnitude)
    };
    shifted.unwrap_or(crate::sys::time::UNIX_EPOCH)
}

impl FileAttr {
    fn from_metadata(metadata: ask_io::fs::Metadata) -> FileAttr {
        FileAttr {
            size: metadata.size,
            mode: metadata.mode,
            is_dir: metadata.kind == ask_io::fs::NodeKind::Directory as u8,
            times: Some(FileStatTimes {
                accessed_sec: metadata.atime_ns / 1_000_000_000,
                modified_sec: metadata.mtime_ns / 1_000_000_000,
            }),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions {
            readonly: self.mode & MODE_OWNER_WRITE == 0,
        }
    }

    pub fn file_type(&self) -> FileType {
        FileType {
            is_dir: self.is_dir,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        self.times
            .map(|t| system_time_from_secs(t.modified_sec))
            .ok_or_else(unsupported_err)
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        self.times
            .map(|t| system_time_from_secs(t.accessed_sec))
            .ok_or_else(unsupported_err)
    }

    /// `askfs` stores a change time, not a creation time, so no value here
    /// would satisfy `std`'s "created" contract.
    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }

    pub fn write(&mut self, write: bool) {
        self.write = write;
    }

    pub fn append(&mut self, append: bool) {
        self.append = append;
    }

    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }

    pub fn create(&mut self, create: bool) {
        self.create = create;
    }

    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }

    fn wire_flags(&self) -> u32 {
        let mut flags = 0;
        if self.create || self.create_new {
            flags |= ask_io::fs::OPEN_CREATE;
        }
        if self.truncate {
            flags |= ask_io::fs::OPEN_TRUNCATE;
        }
        if self.append {
            flags |= ask_io::fs::OPEN_APPEND;
        }
        if self.create_new {
            flags |= ask_io::fs::OPEN_EXCL;
        }
        flags
    }
}

/// One open file's mutable state: the `askfs` channel plus the client-side
/// cursor/size tracking `askio::fs::File` also keeps, since `askfs`'s wire
/// protocol carries no server-side "current position" concept of its own.
struct Inner {
    channel: SyncChannel,
    position: u64,
    size: u64,
}

/// A single open file with its own `SyncChannel` and handle. The mutex
/// serializes wire operations and cursor updates when callers share the file.
pub struct File {
    inner: Mutex<Inner>,
    handle: u32,
    append: bool,
}

fn map_fs_result(result: i32) -> io::Result<()> {
    if result < 0 { Err(unsupported_err()) } else { Ok(()) }
}

/// Metadata/`OP_UNLINK`-shaped requests carry bare path bytes; every one is
/// bounded by `STAT_PATH_MAX` rather than `OPEN_PATH_MAX`.
fn bounded_path(path: &[u8]) -> io::Result<&[u8]> {
    if path.len() > ask_io::fs::STAT_PATH_MAX {
        return Err(unsupported_err());
    }
    Ok(path)
}

/// One path-addressed round trip. `askfs`'s stateless metadata operations
/// (metadata, `MKDIR`, `UNLINK`, `RENAME`, directory enumeration, ...) need no open
/// handle, but each still requires an accepted session because the provider
/// serves only one client Channel at a time.
fn metadata_call<T>(
    provider_token: u32,
    op: u32,
    payload: &[u8],
    not_found: &'static str,
    decode: impl FnOnce(&[u8]) -> Option<T>,
) -> io::Result<T> {
    let mut channel = SyncChannel::create_leased(provider_token, 1)
        .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;
    let completion = channel.call(op, payload)?;
    if completion.result < 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, not_found));
    }
    decode(completion.payload()).ok_or_else(unsupported_err)
}

/// A metadata round trip whose reply carries no payload beyond its result.
fn metadata_unit(
    provider_token: u32,
    op: u32,
    payload: &[u8],
    failure: &'static str,
) -> io::Result<()> {
    let mut channel = SyncChannel::create_leased(provider_token, 1)
        .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;
    let completion = channel.call(op, payload)?;
    if completion.result < 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, failure));
    }
    Ok(())
}

/// Shared by `stat` and `lstat` — `askfs` resolves symlinks provider-side and
/// exposes no nofollow variant, so both spellings observe the same target.
fn stat_path(path: &Path) -> io::Result<FileAttr> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    let payload = bounded_path(relative.as_bytes())?;
    metadata_call(
        provider_token,
        ask_io::fs::OP_METADATA,
        payload,
        "fs: stat failed",
        |reply| ask_io::fs::decode_fs_metadata(reply).map(FileAttr::from_metadata),
    )
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let (provider_token, relative) = resolve_fs_path(path)?;
        let mut channel = SyncChannel::create_leased(provider_token, 1)
            .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;

        let mut request = [0u8; 4 + ask_io::fs::OPEN_PATH_MAX];
        let payload =
            ask_io::fs::encode_fs_open_request(&mut request, opts.wire_flags(), relative.as_bytes())
                .ok_or_else(unsupported_err)?;
        let completion = channel.call(ask_io::fs::OP_OPEN, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::NotFound, "fs: open failed"));
        }
        let (handle, size) =
            ask_io::fs::decode_fs_open_reply(completion.payload()).ok_or_else(unsupported_err)?;
        if handle == ask_io::fs::HANDLE_INVALID {
            return Err(io::const_error!(io::ErrorKind::NotFound, "fs: open failed"));
        }

        Ok(File {
            inner: Mutex::new(Inner {
                channel,
                position: if opts.append { size } else { 0 },
                size,
            }),
            handle,
            append: opts.append,
        })
    }

    /// The open handle carries only the size `FS_OP_OPEN` returned; `askfs`
    /// has no handle-addressed stat, and reopening a path-addressed
    /// metadata session here would race the caller's own writes.
    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(FileAttr {
            size: inner.size,
            mode: MODE_OWNER_WRITE,
            is_dir: false,
            times: None,
        })
    }

    pub fn fsync(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn datasync(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn truncate(&self, size: u64) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut request = [0u8; 12];
        let payload = ask_io::fs::encode_fs_ftruncate_request(&mut request, self.handle, size);
        let completion = inner.channel.call(ask_io::fs::OP_FTRUNCATE, payload)?;
        map_fs_result(completion.result)?;
        inner.size = size;
        if inner.position > size {
            inner.position = size;
        }
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let want = buf.len().min(ask_io::fs::READ_DATA_MAX);
        let mut request = [0u8; 16];
        let payload = ask_io::fs::encode_fs_read_request(
            &mut request,
            self.handle,
            inner.position,
            want as u32,
        );
        let completion = inner.channel.call(ask_io::fs::OP_READ, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "askfs: read failed"));
        }
        let data = completion.payload();
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        inner.position += n as u64;
        Ok(n)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|buf| self.read(buf), cursor)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let n = buf.len().min(ask_io::fs::WRITE_DATA_MAX);
        // `askfs` always lands an append-mode write at the tree's live EOF,
        // ignoring the declared offset — declaring `size` here (not
        // `position`) matches that server behavior, mirroring
        // `askio::fs::File`'s own append handling.
        let offset = if self.append { inner.size } else { inner.position };
        let mut request = [0u8; 12 + ask_io::fs::WRITE_DATA_MAX];
        let payload =
            ask_io::fs::encode_fs_write_request(&mut request, self.handle, offset, &buf[..n])
                .ok_or_else(unsupported_err)?;
        let completion = inner.channel.call(ask_io::fs::OP_WRITE, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "askfs: write failed"));
        }
        let written = ask_io::fs::decode_fs_handle(completion.payload())
            .ok_or_else(unsupported_err)? as usize;
        let written = written.min(n);
        inner.position = offset + written as u64;
        if inner.position > inner.size {
            inner.size = inner.position;
        }
        Ok(written)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let new_position = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(delta) => {
                let base = inner.size as i64;
                base.checked_add(delta)
                    .filter(|v| *v >= 0)
                    .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "seek out of range"))?
                    as u64
            }
            SeekFrom::Current(delta) => {
                let base = inner.position as i64;
                base.checked_add(delta)
                    .filter(|v| *v >= 0)
                    .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "seek out of range"))?
                    as u64
            }
        };
        inner.position = new_position;
        Ok(new_position)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Some(Ok(inner.size))
    }

    pub fn tell(&self) -> io::Result<u64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(inner.position)
    }

    pub fn duplicate(&self) -> io::Result<File> {
        unsupported()
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        unsupported()
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }

    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn try_lock(&self) -> Result<(), crate::fs::TryLockError> {
        Err(crate::fs::TryLockError::Error(io::Error::from(io::ErrorKind::Unsupported)))
    }

    pub fn try_lock_shared(&self) -> Result<(), crate::fs::TryLockError> {
        Err(crate::fs::TryLockError::Error(io::Error::from(io::ErrorKind::Unsupported)))
    }

    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let mut request = [0u8; 4];
        let payload = ask_io::fs::encode_fs_handle(&mut request, self.handle);
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = inner.channel.call(ask_io::fs::OP_CLOSE, payload);
    }
}

impl crate::fmt::Debug for File {
    fn fmt(&self, f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        f.debug_struct("File").field("handle", &self.handle).finish()
    }
}

#[derive(Debug)]
pub struct DirBuilder {}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    /// `askfs` masks the mode to its permission bits server-side; `std`'s
    /// `DirBuilder` exposes no mode on this target, so request the usual
    /// owner-writable directory permissions.
    pub fn mkdir(&self, path: &Path) -> io::Result<()> {
        let (provider_token, relative) = resolve_fs_path(path)?;
        let mut request = [0u8; 4 + ask_io::fs::STAT_PATH_MAX];
        let payload = ask_io::fs::encode_fs_mkdir_request(
            &mut request,
            DEFAULT_DIR_MODE,
            relative.as_bytes(),
        )
        .ok_or_else(unsupported_err)?;
        metadata_unit(
            provider_token,
            ask_io::fs::OP_MKDIR,
            payload,
            "fs: mkdir failed",
        )
    }
}

/// Directory enumeration is stateless and opaque-cookie-addressed.
pub struct ReadDir {
    provider_token: u32,
    relative: heapless_path::PathBuf,
    /// The `/mount/...` spelling the caller passed, so `DirEntry::path()` can
    /// return a path in the caller's own namespace rather than a
    /// provider-relative one.
    root: PathBuf,
    cookie: ask_io::fs::DirCookie,
    exhausted: bool,
}

impl crate::fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        f.debug_struct("ReadDir").field("root", &self.root).finish()
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        if self.exhausted {
            return None;
        }
        let mut request = [0u8; 16 + ask_io::fs::STAT_PATH_MAX];
        let payload = match ask_io::fs::encode_fs_directory_request(
            &mut request,
            self.cookie,
            self.relative.as_bytes(),
        ) {
            Some(payload) => payload,
            None => {
                self.exhausted = true;
                return Some(Err(unsupported_err()));
            }
        };

        let mut channel = match SyncChannel::create_leased(self.provider_token, 1) {
            Ok(channel) => channel,
            Err(_) => {
                self.exhausted = true;
                return Some(Err(io::const_error!(
                    io::ErrorKind::NotFound,
                    "fs provider unreachable"
                )));
            }
        };
        let completion = match channel.call(ask_io::fs::OP_READ_DIRECTORY, payload) {
            Ok(completion) => completion,
            Err(e) => {
                self.exhausted = true;
                return Some(Err(e));
            }
        };
        if completion.result == ask_io::fs::READDIR_RESULT_EOF {
            self.exhausted = true;
            return None;
        }
        if completion.result != 0 {
            self.exhausted = true;
            return Some(Err(io::const_error!(io::ErrorKind::Other, "fs: readdir failed")));
        }
        let (next, _, kind, name) =
            match ask_io::fs::decode_fs_directory_reply(completion.payload()) {
                Some(entry) => entry,
                None => {
                    self.exhausted = true;
                    return Some(Err(unsupported_err()));
                }
            };
        if next.generation == 0 || next == self.cookie {
            self.exhausted = true;
            return Some(Err(unsupported_err()));
        }
        self.cookie = next;

        let name = match crate::str::from_utf8(name) {
            Ok(name) => name,
            Err(_) => {
                self.exhausted = true;
                return Some(Err(unsupported_err()));
            }
        };
        Some(Ok(DirEntry {
            path: self.root.join(name),
            name: OsString::from(name),
            is_dir: kind == ask_io::fs::NodeKind::Directory as u8,
        }))
    }
}

pub struct DirEntry {
    path: PathBuf,
    name: OsString,
    is_dir: bool,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    /// A full attribute set requires its own path-addressed metadata query.
    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat_path(&self.path)
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType {
            is_dir: self.is_dir,
        })
    }
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    Ok(ReadDir {
        provider_token,
        relative,
        root: path.to_path_buf(),
        cookie: ask_io::fs::DirCookie::default(),
        exhausted: false,
    })
}

pub fn unlink(path: &Path) -> io::Result<()> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    let payload = bounded_path(relative.as_bytes())?;
    metadata_unit(
        provider_token,
        ask_io::fs::OP_UNLINK,
        payload,
        "fs: unlink failed",
    )
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let (old_provider, old_relative) = resolve_fs_path(old)?;
    let (new_provider, new_relative) = resolve_fs_path(new)?;
    // `FS_OP_RENAME` is one provider's own directory operation; a move across
    // two mounts would need a copy plus unlink, which is the caller's policy
    // to choose, not this PAL's.
    if old_provider != new_provider {
        return Err(io::const_error!(
            io::ErrorKind::CrossesDevices,
            "fs: rename across providers"
        ));
    }
    let mut request = [0u8; ask_io::fs::RENAME_REQUEST_MAX];
    let payload = ask_io::fs::encode_fs_rename_request(
        &mut request,
        old_relative.as_bytes(),
        new_relative.as_bytes(),
    )
    .ok_or_else(unsupported_err)?;
    metadata_unit(
        old_provider,
        ask_io::fs::OP_RENAME,
        payload,
        "fs: rename failed",
    )
}

/// `askfs` removes an empty directory through the same `FS_OP_UNLINK` entry
/// point it uses for files, rejecting a non-empty target provider-side.
pub fn rmdir(path: &Path) -> io::Result<()> {
    unlink(path)
}

/// `askfs` has no recursive-remove operation, so this walks the tree with
/// opaque-cookie directory enumeration and removes depth-first. Each level's
/// entries are read to completion before descending so mutation cannot stale
/// a cookie still needed by that level.
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    for entry in readdir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_dir_all(&entry.path())?;
        } else {
            unlink(&entry.path())?;
        }
    }
    rmdir(path)
}

pub fn exists(path: &Path) -> io::Result<bool> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    match File::open(path, &opts) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn readlink(_path: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    stat_path(path)
}

pub fn lstat(path: &Path) -> io::Result<FileAttr> {
    stat_path(path)
}

pub fn set_perm(_path: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}

pub fn set_times(_path: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn set_times_nofollow(_path: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn canonicalize(_path: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn copy(_from: &Path, _to: &Path) -> io::Result<u64> {
    unsupported()
}
