//! Files retain resident descriptors; path operations use the scoped startup view.

use askrust::fs::{kind, open};

use crate::ffi::OsString;
use crate::hash::Hash;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::os::fd::FromRawFd;
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::{ExtraHomeDirs, ExtraMediaDirs, copy};
use crate::sys::pal::ask_error;
use crate::sys::time::SystemTime;
use crate::sys::unsupported;

/// ASK paths are UTF-8.
fn text(path: &Path) -> io::Result<&str> {
    path.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))
}

/// Reads a variable-length runtime string, growing the buffer until it fits.
pub(crate) fn read_string(read: impl Fn(&mut [u8]) -> askrust::Result<usize>) -> io::Result<String> {
    let mut buffer = Vec::new();
    loop {
        let length = read(&mut buffer).map_err(ask_error)?;
        if length <= buffer.len() {
            buffer.truncate(length);
            return String::from_utf8(buffer).map_err(|_| io::Error::from(io::ErrorKind::InvalidData));
        }
        buffer.resize(length, 0);
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
}

impl FileTimes {
    pub fn set_accessed(&mut self, t: SystemTime) {
        self.accessed = Some(t);
    }

    pub fn set_modified(&mut self, t: SystemTime) {
        self.modified = Some(t);
    }

    fn to_times(self) -> io::Result<askrust::fs::Times> {
        let mut times = askrust::fs::Times::default();
        if let Some(accessed) = self.accessed {
            times.flags |= askrust::fs::times::ACCESSED;
            times.accessed_ns = protocol_ns_from_system_time(accessed)?;
        }
        if let Some(modified) = self.modified {
            times.flags |= askrust::fs::times::MODIFIED;
            times.modified_ns = protocol_ns_from_system_time(modified)?;
        }
        Ok(times)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePermissions {
    mode: u32,
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.mode & MODE_WRITE == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !MODE_WRITE;
        } else {
            self.mode |= MODE_WRITE;
        }
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }

    pub fn from_mode(mode: u32) -> FilePermissions {
        FilePermissions { mode }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileType {
    kind: u8,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.kind == kind::DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        self.kind == kind::FILE
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == kind::SYMLINK
    }

    pub fn kind(&self) -> u8 {
        self.kind
    }
}

#[derive(Clone)]
pub struct FileAttr {
    metadata: askrust::fs::Metadata,
}

const MODE_WRITE: u32 = 0o222;
const MODE_PERMISSIONS: u32 = 0o7777;

fn system_time_from_protocol_ns(nanoseconds: i64) -> SystemTime {
    let offset = crate::time::Duration::from_nanos(nanoseconds.unsigned_abs());
    let time = if nanoseconds < 0 {
        crate::sys::time::UNIX_EPOCH.checked_sub_duration(&offset)
    } else {
        crate::sys::time::UNIX_EPOCH.checked_add_duration(&offset)
    };
    time.unwrap_or(crate::sys::time::UNIX_EPOCH)
}

fn protocol_ns_from_system_time(time: SystemTime) -> io::Result<i64> {
    let nanoseconds = match time.sub_time(&crate::sys::time::UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).ok(),
        Err(before) => i128::try_from(before.as_nanos()).ok().map(|n| -n),
    };
    nanoseconds.and_then(|n| i64::try_from(n).ok()).ok_or_else(|| {
        io::const_error!(io::ErrorKind::InvalidInput, "fs: timestamp beyond the protocol range")
    })
}

impl FileAttr {
    fn from_metadata(metadata: askrust::fs::Metadata) -> FileAttr {
        FileAttr { metadata }
    }

    pub(crate) fn native(&self) -> &askrust::fs::Metadata {
        &self.metadata
    }

    pub fn size(&self) -> u64 {
        self.metadata.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions { mode: self.metadata.mode & MODE_PERMISSIONS }
    }

    pub fn file_type(&self) -> FileType {
        FileType { kind: self.metadata.kind }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(system_time_from_protocol_ns(self.metadata.modified_ns))
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(system_time_from_protocol_ns(self.metadata.accessed_ns))
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        if self.metadata.flags & askrust::fs::metadata_flags::BIRTH_TIME == 0 {
            return unsupported();
        }
        Ok(system_time_from_protocol_ns(self.metadata.birth_ns))
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    flags: u32,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions { flags: 0 }
    }

    fn set(&mut self, flag: u32, value: bool) {
        if value {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }

    pub fn nofollow(&mut self, nofollow: bool) {
        self.set(open::NOFOLLOW, nofollow);
    }

    pub fn read(&mut self, read: bool) {
        self.set(open::READ, read);
    }

    pub fn write(&mut self, write: bool) {
        self.set(open::WRITE, write);
    }

    pub fn append(&mut self, append: bool) {
        self.set(open::APPEND, append);
    }

    pub fn truncate(&mut self, truncate: bool) {
        self.set(open::TRUNCATE, truncate);
    }

    pub fn create(&mut self, create: bool) {
        self.set(open::CREATE, create);
    }

    pub fn create_new(&mut self, create_new: bool) {
        self.set(open::CREATE_NEW, create_new);
    }
}

pub struct File {
    descriptor: crate::sys::fd::FileDesc,
}

impl File {
    fn from_raw(fd: i32) -> File {
        // SAFETY: the runtime returned a newly bound descriptor owned by the caller.
        File { descriptor: unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) } }
    }

    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        askrust::fs::open(text(path)?, opts.flags).map(File::from_raw).map_err(ask_error)
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.descriptor.as_raw_fd()
    }
    pub fn file_attr(&self) -> io::Result<FileAttr> {
        askrust::fs::fd_metadata(self.as_raw_fd()).map(FileAttr::from_metadata).map_err(ask_error)
    }
    pub fn fsync(&self) -> io::Result<()> {
        self.descriptor.flush()
    }
    pub fn datasync(&self) -> io::Result<()> {
        self.descriptor.flush()
    }
    pub fn truncate(&self, size: u64) -> io::Result<()> {
        askrust::fs::truncate(self.as_raw_fd(), size).map_err(ask_error)
    }
    pub fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        self.descriptor.read(bytes)
    }
    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.descriptor.read_vectored(bufs)
    }
    pub fn is_read_vectored(&self) -> bool {
        self.descriptor.is_read_vectored()
    }
    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        self.descriptor.read_buf(cursor)
    }
    pub fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.descriptor.write(bytes)
    }
    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        self.descriptor.write_vectored(bufs)
    }
    pub fn is_write_vectored(&self) -> bool {
        self.descriptor.is_write_vectored()
    }
    pub fn flush(&self) -> io::Result<()> {
        self.descriptor.flush()
    }
    pub fn seek(&self, position: SeekFrom) -> io::Result<u64> {
        use askrust::fs::whence;
        let (whence, offset) = match position {
            SeekFrom::Start(offset) => {
                (whence::START, i64::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?)
            }
            SeekFrom::Current(delta) => (whence::CURRENT, delta),
            SeekFrom::End(delta) => (whence::END, delta),
        };
        askrust::fs::seek(self.as_raw_fd(), whence, offset).map_err(ask_error)
    }
    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|attribute| attribute.size()))
    }
    pub fn tell(&self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
    pub fn duplicate(&self) -> io::Result<Self> {
        self.descriptor.try_clone().map(|descriptor| Self { descriptor })
    }
    pub fn set_permissions(&self, permissions: FilePermissions) -> io::Result<()> {
        askrust::fs::fd_chmod(self.as_raw_fd(), permissions.mode).map_err(ask_error)
    }
    pub fn set_times(&self, times: FileTimes) -> io::Result<()> {
        askrust::fs::fd_set_times(self.as_raw_fd(), &times.to_times()?).map_err(ask_error)
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
impl crate::os::fd::AsFd for File {
    fn as_fd(&self) -> crate::os::fd::BorrowedFd<'_> {
        crate::os::fd::AsFd::as_fd(&self.descriptor)
    }
}
impl crate::sys::AsInner<crate::sys::fd::FileDesc> for File {
    fn as_inner(&self) -> &crate::sys::fd::FileDesc {
        &self.descriptor
    }
}
impl crate::sys::IntoInner<crate::sys::fd::FileDesc> for File {
    fn into_inner(self) -> crate::sys::fd::FileDesc {
        self.descriptor
    }
}
impl crate::sys::FromInner<crate::sys::fd::FileDesc> for File {
    fn from_inner(descriptor: crate::sys::fd::FileDesc) -> Self {
        Self { descriptor }
    }
}
impl crate::fmt::Debug for File {
    fn fmt(&self, f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        f.debug_struct("File").field("fd", &self.as_raw_fd()).finish()
    }
}

pub struct Dir {
    file: File,
    provider: u32,
}

impl Dir {
    fn runtime(&self) -> askrust::fs::Dir {
        askrust::fs::Dir { fd: self.file.as_raw_fd(), provider: self.provider }
    }

    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<Self> {
        let dir = askrust::fs::dir_open(text(path)?, opts.flags).map_err(ask_error)?;
        Ok(Dir { file: File::from_raw(dir.fd), provider: dir.provider })
    }

    pub fn open_for_traversal(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        Dir::open(path, &opts)
    }

    pub fn duplicate(&self) -> io::Result<Self> {
        Ok(Dir { file: self.file.duplicate()?, provider: self.provider })
    }

    pub fn open_file(&self, path: &Path, opts: &OpenOptions) -> io::Result<File> {
        askrust::fs::dir_open_at(self.runtime(), text(path)?, opts.flags, false)
            .map(File::from_raw)
            .map_err(ask_error)
    }

    pub fn open_dir(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self> {
        let fd = askrust::fs::dir_open_at(self.runtime(), text(path)?, opts.flags, true)
            .map_err(ask_error)?;
        Ok(Dir { file: File::from_raw(fd), provider: self.provider })
    }

    pub fn self_metadata(&self) -> io::Result<FileAttr> {
        self.file.file_attr()
    }

    pub fn as_fd(&self) -> crate::os::fd::BorrowedFd<'_> {
        crate::os::fd::AsFd::as_fd(&self.file)
    }

    pub fn metadata(&self, path: &Path) -> io::Result<FileAttr> {
        askrust::fs::dir_metadata(self.runtime(), text(path)?, false)
            .map(FileAttr::from_metadata)
            .map_err(ask_error)
    }

    pub fn symlink_metadata(&self, path: &Path) -> io::Result<FileAttr> {
        askrust::fs::dir_metadata(self.runtime(), text(path)?, true)
            .map(FileAttr::from_metadata)
            .map_err(ask_error)
    }

    pub fn entry_names(&self) -> io::Result<crate::vec::Vec<OsString>> {
        let listing = Listing(askrust::fs::dir_read(self.runtime()).map_err(ask_error)?);
        let mut names = crate::vec::Vec::new();
        while let Some((name, _)) = listing.next_entry()? {
            names.push(OsString::from(name));
        }
        Ok(names)
    }

    pub fn set_permissions_at(
        &self,
        path: &Path,
        perm: FilePermissions,
        follow: bool,
    ) -> io::Result<()> {
        askrust::fs::dir_chmod(self.runtime(), text(path)?, perm.mode, !follow).map_err(ask_error)
    }

    pub fn remove_file(&self, _path: &Path) -> io::Result<()> {
        unsupported()
    }

    pub fn rename(&self, _from: &Path, _to_dir: &Self, _to: &Path) -> io::Result<()> {
        unsupported()
    }

    pub fn create_dir(&self, _path: &Path) -> io::Result<()> {
        unsupported()
    }

    pub fn remove_dir(&self, _path: &Path) -> io::Result<()> {
        unsupported()
    }
}

impl crate::fmt::Debug for Dir {
    fn fmt(&self, f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        f.debug_struct("Dir").field("fd", &self.file.as_raw_fd()).finish()
    }
}

#[derive(Debug)]
pub struct DirBuilder {}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    pub fn mkdir(&self, path: &Path) -> io::Result<()> {
        askrust::fs::mkdir(text(path)?).map_err(ask_error)
    }
}

/// A runtime directory listing, closed on drop.
struct Listing(u64);

impl Listing {
    fn next_entry(&self) -> io::Result<Option<(String, u8)>> {
        let mut buffer = Vec::new();
        loop {
            match askrust::fs::read_dir_next(self.0, &mut buffer).map_err(ask_error)? {
                None => return Ok(None),
                Some((length, kind)) if length <= buffer.len() => {
                    buffer.truncate(length);
                    let name = String::from_utf8(buffer)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                    return Ok(Some((name, kind)));
                }
                Some((length, _)) => buffer.resize(length, 0),
            }
        }
    }
}

impl Drop for Listing {
    fn drop(&mut self) {
        askrust::fs::read_dir_close(self.0);
    }
}

pub struct ReadDir {
    listing: Listing,
    root: PathBuf,
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
        match self.listing.next_entry() {
            Ok(Some((name, kind))) => {
                Some(Ok(DirEntry { path: self.root.join(&name), name: OsString::from(name), kind }))
            }
            Ok(None) => {
                self.exhausted = true;
                None
            }
            Err(error) => {
                self.exhausted = true;
                Some(Err(error))
            }
        }
    }
}

pub struct DirEntry {
    path: PathBuf,
    name: OsString,
    kind: u8,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        lstat(&self.path)
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType { kind: self.kind })
    }
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let handle = askrust::fs::read_dir(text(path)?).map_err(ask_error)?;
    Ok(ReadDir { listing: Listing(handle), root: path.to_path_buf(), exhausted: false })
}

pub fn unlink(path: &Path) -> io::Result<()> {
    askrust::fs::unlink(text(path)?).map_err(ask_error)
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    askrust::fs::rename(text(old)?, text(new)?).map_err(ask_error)
}

pub fn rename_noreplace(_old: &Path, _new: &Path) -> io::Result<()> {
    unsupported()
}

pub fn rmdir(path: &Path) -> io::Result<()> {
    askrust::fs::rmdir(text(path)?).map_err(ask_error)
}

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

pub fn readlink(path: &Path) -> io::Result<PathBuf> {
    let path = text(path)?;
    read_string(|buffer| askrust::fs::readlink(path, buffer)).map(PathBuf::from)
}

pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    askrust::fs::symlink(text(original)?, original.is_absolute(), text(link)?).map_err(ask_error)
}

pub fn link(original: &Path, link: &Path) -> io::Result<()> {
    askrust::fs::link(text(original)?, text(link)?).map_err(ask_error)
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    askrust::fs::metadata(text(path)?, false).map(FileAttr::from_metadata).map_err(ask_error)
}

pub fn lstat(path: &Path) -> io::Result<FileAttr> {
    askrust::fs::metadata(text(path)?, true).map(FileAttr::from_metadata).map_err(ask_error)
}

pub fn set_perm(path: &Path, perm: FilePermissions) -> io::Result<()> {
    askrust::fs::chmod(text(path)?, perm.mode).map_err(ask_error)
}

pub use set_perm as set_perm_nofollow;

pub fn set_times(path: &Path, times: FileTimes) -> io::Result<()> {
    askrust::fs::set_times(text(path)?, false, &times.to_times()?).map_err(ask_error)
}

pub fn set_times_nofollow(path: &Path, times: FileTimes) -> io::Result<()> {
    askrust::fs::set_times(text(path)?, true, &times.to_times()?).map_err(ask_error)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let path = text(path)?;
    read_string(|buffer| askrust::fs::canonicalize(path, buffer)).map(PathBuf::from)
}
