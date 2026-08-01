//! `std::fs` PAL for ask: a blocking `File` bridging onto `askfs`'s
//! `FS_OP_*` wire protocol (`ask_abi::fs`) over a `SyncChannel`
//! (`sys::pal::ask::channel`, re-exported as `sys::channel`). One fresh
//! channel per `File::open` — `askfs`
//! itself allows only one open handle per accepted session
//! (`MAX_OPEN_HANDLES = 1`), so multiplexing several files over one channel
//! isn't possible even in principle.
//!
//! The process's `/exe`/`/in`/`/out` bindings arrive once at spawn time in
//! the 64-byte startup view (`GetSpawnBlob(SPAWN_BLOB_VIEW,..)`,
//! `ask_abi::view`); this module decodes mount provider pids and optional
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

/// Decode a path mount's provider pid and optional source root from the
/// startup-view blob — a trimmed, `libask`-free equivalent of
/// `libask::view::View::decode_startup`.
fn mount_binding(slot: usize) -> io::Result<(u32, [u8; ask_abi::view::ROOT_TABLE_LEN], u8, u8)> {
    let mut bytes = [0u8; ask_abi::view::LEN];
    ask_abi::get_startup_view(&mut bytes).map_err(crate::sys::map_ask_error)?;
    let offset = slot * ask_abi::view::BINDING_LEN;
    let bound = *bytes.get(offset + 5).ok_or_else(unsupported_err)?;
    if bound == 0 {
        return Err(unsupported_err());
    }
    let pid = bytes
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(unsupported_err)?;
    let root_off = *bytes.get(offset + 6).ok_or_else(unsupported_err)?;
    let root_len = *bytes.get(offset + 7).ok_or_else(unsupported_err)?;
    let mut root_table = [0u8; ask_abi::view::ROOT_TABLE_LEN];
    let table = bytes
        .get(
            ask_abi::view::ROOT_TABLE_OFFSET
                ..ask_abi::view::ROOT_TABLE_OFFSET + ask_abi::view::ROOT_TABLE_LEN,
        )
        .ok_or_else(unsupported_err)?;
    root_table.copy_from_slice(table);
    Ok((pid, root_table, root_off, root_len))
}

/// Map `/exe|in|out/...` to `(provider_pid, provider-relative path bytes)`.
fn resolve_fs_path(path: &Path) -> io::Result<(u32, heapless_path::PathBuf)> {
    let path = path.to_str().ok_or_else(unsupported_err)?;
    let relative = path.strip_prefix('/').ok_or_else(unsupported_err)?;
    let (mount, rest) = relative.split_once('/').ok_or_else(unsupported_err)?;
    if rest.is_empty()
        || rest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(unsupported_err());
    }
    let slot = match mount {
        "exe" => 0usize,
        "in" => 1,
        "out" => 2,
        _ => return Err(unsupported_err()),
    };
    let (pid, root_table, root_off, root_len) = mount_binding(slot)?;
    let mut out = heapless_path::PathBuf::new();
    if root_len != 0 {
        let start = root_off as usize;
        let end = start
            .checked_add(root_len as usize)
            .ok_or_else(unsupported_err)?;
        let root = root_table.get(start..end).ok_or_else(unsupported_err)?;
        out.push(root)?;
        out.push(b"/")?;
    }
    out.push(rest.as_bytes())?;
    Ok((pid, out))
}

/// Tiny fixed path builder — avoids `alloc` in the hot open path while still
/// fitting `ask_abi::fs::OPEN_PATH_MAX`.
mod heapless_path {
    use super::*;

    pub struct PathBuf {
        buf: [u8; ask_abi::fs::OPEN_PATH_MAX],
        len: usize,
    }

    impl PathBuf {
        pub fn new() -> Self {
            Self {
                buf: [0; ask_abi::fs::OPEN_PATH_MAX],
                len: 0,
            }
        }

        pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
            let end = self
                .len
                .checked_add(bytes.len())
                .ok_or_else(unsupported_err)?;
            if end > ask_abi::fs::OPEN_PATH_MAX {
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

    pub fn is_symlink(&self) -> bool {
        false
    }
}

/// `askfs` reports only a file size in `FS_OP_OPEN`'s reply — no
/// permissions/timestamps/type byte exist on the wire yet
/// (docs/vfs-layout.md). Every open target is reported as a regular,
/// writable file; `askfs` has no directory-open path for `std::fs::File` to
/// observe as `FileType::is_dir()` in the first place.
#[derive(Clone)]
pub struct FileAttr {
    size: u64,
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }

    pub fn file_type(&self) -> FileType {
        FileType { is_dir: false }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        unsupported()
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }

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
            flags |= ask_abi::fs::OPEN_CREATE;
        }
        if self.truncate {
            flags |= ask_abi::fs::OPEN_TRUNCATE;
        }
        if self.append {
            flags |= ask_abi::fs::OPEN_APPEND;
        }
        if self.create_new {
            flags |= ask_abi::fs::OPEN_EXCL;
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

/// A single open file: its own `SyncChannel` to `askfs` (one handle per
/// session — see the module doc comment). `std::fs::File`'s trait contract
/// takes every operation through `&self` (it's `Sync`-shared via `Arc` in
/// some callers), so the channel and cursor live behind a `Mutex` — this PAL
/// still only ever drives one wire round trip at a time per `File`, matching
/// `askfs`'s own one-handle-per-session posture, but a real lock (not a
/// `!Sync` cell) keeps two threads sharing an `Arc<File>` from racing.
pub struct File {
    inner: Mutex<Inner>,
    handle: u32,
    append: bool,
}

fn map_fs_result(result: i32) -> io::Result<()> {
    if result < 0 { Err(unsupported_err()) } else { Ok(()) }
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let (provider_pid, relative) = resolve_fs_path(path)?;
        let mut channel = SyncChannel::create(provider_pid as u64, 1)
            .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;

        let mut request = [0u8; 4 + ask_abi::fs::OPEN_PATH_MAX];
        let payload =
            ask_abi::fs::encode_fs_open_request(&mut request, opts.wire_flags(), relative.as_bytes())
                .ok_or_else(unsupported_err)?;
        let completion = channel.call(ask_abi::fs::OP_OPEN, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::NotFound, "fs: open failed"));
        }
        let (handle, size) =
            ask_abi::fs::decode_fs_open_reply(completion.payload()).ok_or_else(unsupported_err)?;
        if handle == ask_abi::fs::HANDLE_INVALID {
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

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(FileAttr { size: inner.size })
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
        let payload = ask_abi::fs::encode_fs_ftruncate_request(&mut request, self.handle, size);
        let completion = inner.channel.call(ask_abi::fs::OP_FTRUNCATE, payload)?;
        map_fs_result(completion.result)?;
        inner.size = size;
        if inner.position > size {
            inner.position = size;
        }
        Ok(())
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let want = buf.len().min(ask_abi::fs::WRITE_DATA_MAX + 12);
        let mut request = [0u8; 16];
        let payload = ask_abi::fs::encode_fs_read_request(
            &mut request,
            self.handle,
            inner.position,
            want as u32,
        );
        let completion = inner.channel.call(ask_abi::fs::OP_READ, payload)?;
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
        let n = buf.len().min(ask_abi::fs::WRITE_DATA_MAX);
        // `askfs` always lands an append-mode write at the tree's live EOF,
        // ignoring the declared offset — declaring `size` here (not
        // `position`) matches that server behavior, mirroring
        // `askio::fs::File`'s own append handling.
        let offset = if self.append { inner.size } else { inner.position };
        let mut request = [0u8; 12 + ask_abi::fs::WRITE_DATA_MAX];
        let payload =
            ask_abi::fs::encode_fs_write_request(&mut request, self.handle, offset, &buf[..n])
                .ok_or_else(unsupported_err)?;
        let completion = inner.channel.call(ask_abi::fs::OP_WRITE, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "askfs: write failed"));
        }
        let written = ask_abi::fs::decode_fs_handle(completion.payload())
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
        let payload = ask_abi::fs::encode_fs_handle(&mut request, self.handle);
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = inner.channel.call(ask_abi::fs::OP_CLOSE, payload);
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

    pub fn mkdir(&self, _path: &Path) -> io::Result<()> {
        unsupported()
    }
}

pub struct ReadDir(!);

impl crate::fmt::Debug for ReadDir {
    fn fmt(&self, _f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        self.0
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        self.0
    }
}

pub struct DirEntry(!);

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.0
    }

    pub fn file_name(&self) -> OsString {
        self.0
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        self.0
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        self.0
    }
}

pub fn readdir(_path: &Path) -> io::Result<ReadDir> {
    unsupported()
}

pub fn unlink(_path: &Path) -> io::Result<()> {
    unsupported()
}

pub fn rename(_old: &Path, _new: &Path) -> io::Result<()> {
    unsupported()
}

pub fn rmdir(_path: &Path) -> io::Result<()> {
    unsupported()
}

pub fn remove_dir_all(_path: &Path) -> io::Result<()> {
    unsupported()
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

pub fn stat(_path: &Path) -> io::Result<FileAttr> {
    unsupported()
}

pub fn lstat(_path: &Path) -> io::Result<FileAttr> {
    unsupported()
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
