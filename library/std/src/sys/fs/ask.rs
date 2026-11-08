//! Files retain resident descriptors; path operations use the scoped startup view.

use crate::ffi::OsString;
use crate::hash::Hash;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
use crate::sys::channel::SyncChannel;
pub use crate::sys::fs::common::{ExtraHomeDirs, ExtraMediaDirs, copy};
use crate::sys::pal::unsupported_err;
use crate::sys::time::SystemTime;
use crate::sys::unsupported;

fn startup_view() -> io::Result<ask_io::view::View> {
    let mut bytes = [0u8; ask_abi::view::LEN];
    ask_sys::get_startup_view(&mut bytes).map_err(crate::sys::map_ask_error)?;
    ask_io::view::View::decode_startup(&bytes).map_err(|_| unsupported_err())
}

/// Copies the process working directory, a normalized absolute view path.
pub(crate) fn working_directory(buffer: &mut [u8; ask_io::fs::PATH_MAX]) -> io::Result<&str> {
    let mut length = 0;
    crate::sys::fd::retry(|error| unsafe {
        ask_fd::__askfd_get_cwd(buffer.as_mut_ptr(), buffer.len(), &mut length, error)
    })?;
    let bytes = buffer.get(..length).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    crate::str::from_utf8(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

/// Applies the shared path rule: scheme-less and `file:` paths name the view,
/// and relative paths follow the working directory.
pub(crate) fn absolute_view_path<'b>(
    path: &Path,
    output: &'b mut [u8; ask_io::fs::PATH_MAX],
) -> io::Result<&'b str> {
    use ask_io::path::Error;
    let path = path.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))?;
    let mut cwd = [0u8; ask_io::fs::PATH_MAX];
    let cwd = working_directory(&mut cwd)?;
    ask_io::path::resolve(path, cwd, output).map_err(|error| match error {
        Error::UnsupportedScheme | Error::NonEmptyAuthority => {
            io::const_error!(io::ErrorKind::Unsupported, "path names an unsupported location")
        }
        Error::Invalid => io::const_error!(io::ErrorKind::InvalidInput, "malformed path"),
        Error::InvalidWorkingDirectory => io::Error::from(io::ErrorKind::InvalidData),
        Error::BufferTooSmall => io::Error::from(io::ErrorKind::InvalidFilename),
    })
}

/// Maps a view routing refusal to the closest `io::ErrorKind`.
fn view_error(error: ask_io::view::Error) -> io::Error {
    use ask_io::view::Error;
    match error {
        Error::MissingBinding => io::Error::from(io::ErrorKind::NotFound),
        Error::ReadOnly => io::Error::from(io::ErrorKind::ReadOnlyFilesystem),
        Error::BufferTooSmall | Error::RootTableFull => {
            io::Error::from(io::ErrorKind::InvalidFilename)
        }
        Error::InvalidPath | Error::InvalidAccess => io::Error::from(io::ErrorKind::InvalidInput),
        Error::MalformedStartupView => unsupported_err(),
    }
}

/// Map a process path to `(provider_token, provider-relative path bytes)`.
fn resolve_fs_path(path: &Path) -> io::Result<(u32, heapless_path::PathBuf)> {
    let mut absolute = [0u8; ask_io::fs::PATH_MAX];
    resolve_absolute(absolute_view_path(path, &mut absolute)?)
}

/// No provider serves the view root, so every provider operation on it is
/// refused; `stat` and listing answer it locally.
fn resolve_absolute(path: &str) -> io::Result<(u32, heapless_path::PathBuf)> {
    if path == ask_io::path::ROOT {
        return Err(io::const_error!(
            io::ErrorKind::ReadOnlyFilesystem,
            "fs: the view root holds only mount points",
        ));
    }
    let view = startup_view()?;
    let resolved = view.resolve(path, ask_io::view::Operation::Read).map_err(view_error)?;
    let mut encoded = [0u8; ask_io::fs::PATH_MAX];
    let relative = resolved.encode_path(&mut encoded).map_err(|_| unsupported_err())?;
    let mut out = heapless_path::PathBuf::new();
    out.push(relative.as_bytes())?;
    Ok((resolved.provider_token, out))
}

/// Tiny fixed path builder — avoids `alloc` in the hot open path while still
/// fitting `ask_io::fs::PATH_MAX`.
mod heapless_path {
    use super::*;

    pub struct PathBuf {
        buf: [u8; ask_io::fs::PATH_MAX],
        len: usize,
    }

    impl PathBuf {
        pub fn new() -> Self {
            Self { buf: [0; ask_io::fs::PATH_MAX], len: 0 }
        }

        pub fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
            let end = self.len.checked_add(bytes.len()).ok_or_else(unsupported_err)?;
            if end > ask_io::fs::PATH_MAX {
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

/// Timestamps to store; an unset field is omitted from the provider request
/// and keeps its stored value.
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

    fn to_update(self) -> io::Result<ask_io::fs::TimeUpdate> {
        Ok(ask_io::fs::TimeUpdate {
            accessed_ns: self.accessed.map(protocol_ns_from_system_time).transpose()?,
            modified_ns: self.modified.map(protocol_ns_from_system_time).transpose()?,
        })
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

/// The provider's `ask_io::fs::NodeKind` byte, from metadata or a directory entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileType {
    kind: u8,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.kind == ask_io::fs::NodeKind::Directory as u8
    }

    pub fn is_file(&self) -> bool {
        self.kind == ask_io::fs::NodeKind::File as u8
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == ask_io::fs::NodeKind::Symlink as u8
    }

    /// The `ask_io::fs::NodeKind` byte behind `std::os::ask::fs::FileTypeExt`.
    pub fn kind(&self) -> u8 {
        self.kind
    }
}

/// Provider metadata for a path or open handle.
#[derive(Clone)]
pub struct FileAttr {
    metadata: ask_io::fs::Metadata,
}

/// Write bits for every class, cleared and set together by `set_readonly` as
/// on Unix.
const MODE_WRITE: u32 = 0o222;
/// The permission, set-id and sticky bits a native mode carries beside its type.
const MODE_PERMISSIONS: u32 = 0o7777;

/// POSIX creation modes before the process umask removes bits: files are
/// readable and writable by all, directories also searchable.
mod creation_mode {
    pub const FILE: u32 = 0o666;
    pub const DIRECTORY: u32 = 0o777;
}

/// The resident owner's process umask.
pub(crate) fn umask() -> io::Result<u32> {
    let mut mask = 0;
    crate::sys::fd::retry(|error| unsafe { ask_fd::__askfd_get_umask(&mut mask, error) })?;
    Ok(mask)
}

/// The resident owner's umask applied to a creation mode.
fn masked_mode(mode: u32) -> io::Result<u32> {
    Ok(mode & !umask()?)
}

/// Native timestamps are signed nanoseconds from the Unix epoch.
fn system_time_from_protocol_ns(nanoseconds: i64) -> SystemTime {
    let offset = crate::time::Duration::from_nanos(nanoseconds.unsigned_abs());
    let time = if nanoseconds < 0 {
        crate::sys::time::UNIX_EPOCH.checked_sub_duration(&offset)
    } else {
        crate::sys::time::UNIX_EPOCH.checked_add_duration(&offset)
    };
    // An `i64` nanosecond count spans about 292 years, inside `SystemTime`'s range.
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
    fn from_metadata(metadata: ask_io::fs::Metadata) -> FileAttr {
        FileAttr { metadata }
    }

    /// The complete provider record behind `std::os::ask::fs::MetadataExt`.
    pub fn native(&self) -> &ask_io::fs::Metadata {
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
        Ok(system_time_from_protocol_ns(self.metadata.mtime_ns))
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(system_time_from_protocol_ns(self.metadata.atime_ns))
    }

    /// Providers without a birth time leave its metadata flag clear.
    pub fn created(&self) -> io::Result<SystemTime> {
        if self.metadata.flags & ask_io::fs::METADATA_HAS_BIRTH_TIME == 0 {
            return unsupported();
        }
        Ok(system_time_from_protocol_ns(self.metadata.btime_ns))
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
    nofollow: bool,
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
            nofollow: false,
        }
    }

    pub fn nofollow(&mut self, nofollow: bool) {
        self.nofollow = nofollow;
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

    /// A creating open carries the umask-masked file mode.
    fn wire_flags(&self) -> io::Result<u32> {
        let mut flags = 0;
        if self.create || self.create_new {
            flags |= ask_io::fs::OPEN_CREATE
                | ask_io::fs::OPEN_MODE_PRESENT
                | (masked_mode(creation_mode::FILE)? << ask_io::fs::OPEN_MODE_SHIFT);
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
        if self.nofollow {
            flags |= ask_io::fs::OPEN_NOFOLLOW;
        }
        Ok(flags)
    }
}

/// One alias in the resident description owner.
pub struct File {
    descriptor: crate::sys::fd::FileDesc,
}

fn map_fs_result(result: i32, context: &'static str) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(fs_error(result, context)) }
}

/// Translate a positive filesystem refusal into its typed error kind.
/// Unknown codes, including negative transport failures, remain `Other`.
pub(crate) fn fs_error(result: i32, context: &'static str) -> io::Error {
    use ask_io::fs::FsError;

    let kind = match FsError::from_result(result) {
        Some(FsError::NotFound) => io::ErrorKind::NotFound,
        Some(FsError::NotDirectory) => io::ErrorKind::NotADirectory,
        Some(FsError::IsDirectory) => io::ErrorKind::IsADirectory,
        Some(FsError::PermissionDenied) => io::ErrorKind::PermissionDenied,
        Some(FsError::ReadOnlyFilesystem) => io::ErrorKind::ReadOnlyFilesystem,
        Some(FsError::AlreadyExists) => io::ErrorKind::AlreadyExists,
        Some(FsError::DirectoryNotEmpty) => io::ErrorKind::DirectoryNotEmpty,
        Some(FsError::CrossDevice) => io::ErrorKind::CrossesDevices,
        Some(FsError::Invalid) => io::ErrorKind::InvalidInput,
        Some(FsError::NameTooLong) => io::ErrorKind::InvalidFilename,
        Some(FsError::NoSpace) => io::ErrorKind::StorageFull,
        Some(FsError::QuotaExceeded) => io::ErrorKind::QuotaExceeded,
        Some(FsError::Busy) => io::ErrorKind::ResourceBusy,
        Some(FsError::StaleHandle) => io::ErrorKind::StaleNetworkFileHandle,
        Some(FsError::Unsupported) => io::ErrorKind::Unsupported,
        Some(FsError::Cancelled) => io::ErrorKind::Interrupted,
        Some(FsError::Io) | Some(FsError::Malformed) | None => io::ErrorKind::Other,
    };
    io::Error::new(kind, context)
}

/// Open a session with the provider's path window, stage one path request
/// in it, and exchange it.
fn path_exchange<const N: usize>(
    provider_token: u32,
    op: u32,
    encode: impl FnOnce(&mut [u8; N], &mut [u8]) -> bool,
) -> io::Result<crate::sys::channel::Completion> {
    let mut channel =
        SyncChannel::create_leased(provider_token, ask_io::fs::path_window::CHANNEL_PAGES)
            .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;
    let mut request = [0u8; N];
    let window = channel
        .shared_region_mut(ask_io::fs::path_window::OFFSET, ask_io::fs::path_window::LEN)
        .ok_or_else(unsupported_err)?;
    if !encode(&mut request, window) {
        return Err(io::const_error!(io::ErrorKind::InvalidFilename, "fs: path too long"));
    }
    channel.call(op, &request)
}

/// A path-addressed metadata operation uses its own admitted provider Channel.
fn metadata_call<T, const N: usize>(
    provider_token: u32,
    op: u32,
    encode: impl FnOnce(&mut [u8; N], &mut [u8]) -> bool,
    not_found: &'static str,
    decode: impl FnOnce(&[u8]) -> Option<T>,
) -> io::Result<T> {
    let completion = path_exchange(provider_token, op, encode)?;
    map_fs_result(completion.result, not_found)?;
    decode(completion.payload()).ok_or_else(unsupported_err)
}

/// A metadata round trip whose reply carries no payload beyond its result.
fn metadata_unit<const N: usize>(
    provider_token: u32,
    op: u32,
    encode: impl FnOnce(&mut [u8; N], &mut [u8]) -> bool,
    failure: &'static str,
) -> io::Result<()> {
    let completion = path_exchange(provider_token, op, encode)?;
    map_fs_result(completion.result, failure)?;
    Ok(())
}

/// Shared path query; the provider performs final-symlink traversal according
/// to the request flag in the same namespace operation as metadata lookup.
fn stat_path(path: &Path, nofollow: bool) -> io::Result<FileAttr> {
    let mut absolute = [0u8; ask_io::fs::PATH_MAX];
    let absolute = absolute_view_path(path, &mut absolute)?;
    if absolute == ask_io::path::ROOT {
        return Ok(FileAttr::from_metadata(ask_io::path::root_metadata()));
    }
    let (provider_token, relative) = resolve_absolute(absolute)?;
    metadata_call(
        provider_token,
        ask_io::fs::OP_METADATA,
        |request: &mut [u8; ask_io::fs::FS_METADATA_PATH_REQUEST_LEN], window| {
            ask_io::fs::encode_fs_metadata_path_request(
                request,
                window,
                relative.as_bytes(),
                ask_io::fs::MetadataLookup::from_nofollow(nofollow),
            )
            .is_some()
        },
        "fs: stat failed",
        |reply| ask_io::fs::decode_fs_metadata(reply).map(FileAttr::from_metadata),
    )
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let (provider_token, relative) = resolve_fs_path(path)?;
        File::open_native(provider_token, None, relative.as_bytes(), opts)
    }

    /// Opens `path` from the provider's session root or, with a directory
    /// token, beneath that directory; both bind a fresh lifetime session.
    fn open_native(
        provider_token: u32,
        directory: Option<u64>,
        path: &[u8],
        opts: &OpenOptions,
    ) -> io::Result<File> {
        crate::sys::fd::blocking(async {
            let source = crate::sys::fd::mount_source(provider_token)?;
            let pages = ask_io::fs::path_window::CHANNEL_PAGES;
            let mut channel = ask_ipc::SyncChannel::create_leased(source.record.token.0, pages)
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            let rights = (if opts.read { ask_io::fd::rights::READ } else { 0 })
                | (if opts.write || opts.append { ask_io::fd::rights::WRITE } else { 0 })
                | ask_io::fd::rights::TRANSFER;
            if rights & ask_io::fd::rights::ACCESS == 0 {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            let mut request = [0; ask_io::fs::FS_OPEN_REQUEST_LEN];
            let mut relative_request = [0; ask_io::fs::FS_OPEN_RELATIVE_REQUEST_LEN];
            let window = channel
                .shared_region_mut(ask_io::fs::path_window::OFFSET, ask_io::fs::path_window::LEN)
                .ok_or_else(unsupported_err)?;
            let flags = opts.wire_flags()?;
            let payload = match directory {
                None => ask_io::fs::encode_fs_open_request(&mut request, window, flags, rights, path),
                Some(token) => ask_io::fs::encode_fs_open_relative_request(
                    &mut relative_request,
                    window,
                    flags,
                    rights,
                    token,
                    path,
                ),
            }
            .ok_or_else(unsupported_err)?;
            let deadline = ask_sys::get_monotonic_ms()
                .checked_add(ask_ipc::directory::policy::DEADLINE_MS)
                .ok_or_else(unsupported_err)?;
            let completion =
                crate::sys::fd::call(&mut channel, ask_io::fs::OP_OPEN, payload, deadline)?;
            map_fs_result(completion.result, "file open failed")?;
            let reply = ask_io::fs::decode_fs_open_reply(completion.payload())
                .filter(|reply| reply.granted_rights == rights)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            let descriptor = crate::sys::fd::bind_native(source, channel, pages, reply)?;
            Ok(File { descriptor })
        })?
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.descriptor.as_raw_fd()
    }
    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let mut bytes = [0; ask_io::fs::METADATA_LEN];
        let result =
            crate::sys::fd::blocking(crate::sys::fd::metadata(self.as_raw_fd(), &mut bytes))??;
        if result.encoding != ask_fd::metadata_encoding::FILESYSTEM
            || result.byte_length != bytes.len() as u64
        {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        ask_io::fs::decode_fs_metadata(&bytes)
            .map(FileAttr::from_metadata)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
    }
    pub fn fsync(&self) -> io::Result<()> {
        self.descriptor.flush()
    }
    pub fn datasync(&self) -> io::Result<()> {
        self.descriptor.flush()
    }
    pub fn truncate(&self, size: u64) -> io::Result<()> {
        let mut bytes = [0; ask_io::fs::FS_FTRUNCATE_REQUEST_LEN];
        let payload = ask_io::fs::encode_fs_ftruncate_request(&mut bytes, size);
        crate::sys::fd::blocking(crate::sys::fd::native(
            self.as_raw_fd(),
            ask_io::fs::OP_FTRUNCATE,
            payload,
            &mut [],
        ))?
        .map(|_| ())
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
        let (position, absolute_offset, seek_delta) = match position {
            SeekFrom::Start(offset) => (ask_fd::position::ABSOLUTE, offset, 0),
            SeekFrom::Current(delta) => (ask_fd::position::DESCRIPTION_CURSOR, 0, delta),
            SeekFrom::End(delta) => (ask_fd::position::END, 0, delta),
        };
        let request = ask_fd::IoRequest {
            action: ask_fd::action::SEEK,
            position,
            absolute_offset,
            seek_delta,
            requested_count: 0,
            native_opcode: 0,
            flags: 0,
            endpoint: ask_io::net::SocketAddress::EMPTY,
        };
        crate::sys::fd::blocking(crate::sys::fd::perform(self.as_raw_fd(), request, &[], &mut []))?
            .map(|result| result.resulting_position)
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
        let mut bytes = [0; ask_io::fs::FS_CHMOD_ATTACHMENT_REQUEST_LEN];
        let payload = ask_io::fs::encode_fs_chmod_attachment_request(&mut bytes, permissions.mode);
        crate::sys::fd::blocking(crate::sys::fd::native(
            self.as_raw_fd(),
            ask_io::fs::OP_CHMOD,
            payload,
            &mut [],
        ))?
        .map(|_| ())
    }
    pub fn set_times(&self, times: FileTimes) -> io::Result<()> {
        let mut bytes = [0; ask_io::fs::FS_UTIMENS_ATTACHMENT_REQUEST_LEN];
        let payload =
            ask_io::fs::encode_fs_utimens_attachment_request(&mut bytes, times.to_update()?);
        crate::sys::fd::blocking(crate::sys::fd::native(
            self.as_raw_fd(),
            ask_io::fs::OP_UTIMENS,
            payload,
            &mut [],
        ))?
        .map(|_| ())
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

/// An open directory: its descriptor, and its mount's provider token so a
/// relative open binds a session on the same provider.
pub struct Dir {
    file: File,
    provider_token: u32,
}

/// A name beneath a `Dir`: the provider grammar is descendant-only, so an
/// absolute path has no meaning relative to the directory.
fn beneath(path: &Path) -> io::Result<&[u8]> {
    let path = path.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidFilename))?;
    if path.starts_with('/') || path.len() > ask_io::fs::PATH_MAX {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(path.as_bytes())
}

impl Dir {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<Self> {
        let (provider_token, relative) = resolve_fs_path(path)?;
        Dir::from_file(File::open_native(provider_token, None, relative.as_bytes(), opts)?, provider_token)
    }

    pub fn open_for_traversal(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        Dir::open(path, &opts)
    }

    fn from_file(file: File, provider_token: u32) -> io::Result<Self> {
        if !file.file_attr()?.file_type().is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory));
        }
        Ok(Dir { file, provider_token })
    }

    pub fn duplicate(&self) -> io::Result<Self> {
        Ok(Dir { file: self.file.duplicate()?, provider_token: self.provider_token })
    }

    /// The single-use token a relative open presents; creation needs `WRITE`
    /// on the directory, a lookup `READ`.
    fn token(&self, rights: u32) -> io::Result<u64> {
        let mut request = [0; ask_io::fs::FS_DIRECTORY_TOKEN_REQUEST_LEN];
        let payload = ask_io::fs::encode_fs_directory_token_request(&mut request, rights)
            .ok_or_else(unsupported_err)?;
        let mut reply = [0; ask_io::fs::FS_DIRECTORY_TOKEN_REPLY_LEN];
        let length = crate::sys::fd::blocking(crate::sys::fd::native(
            self.file.as_raw_fd(),
            ask_io::fs::OP_DIRECTORY_TOKEN,
            payload,
            &mut reply,
        ))??;
        reply
            .get(..length)
            .and_then(ask_io::fs::decode_fs_directory_token_reply)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
    }

    /// A relative request carries its path after the request; the resident
    /// owner stages it into the Operation Channel window it alone maps.
    fn relative_native<const N: usize>(
        &self,
        opcode: u32,
        path: &[u8],
        encode: impl FnOnce(&mut [u8; N], &mut [u8]) -> bool,
        output: &mut [u8],
    ) -> io::Result<usize> {
        let mut request = [0; N];
        let mut scratch = [0; ask_io::fs::PATH_MAX];
        if !encode(&mut request, &mut scratch) {
            return Err(io::Error::from(io::ErrorKind::InvalidFilename));
        }
        let mut input = crate::vec::Vec::new();
        input.try_reserve_exact(N + path.len()).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        input.extend_from_slice(&request);
        input.extend_from_slice(path);
        crate::sys::fd::blocking(crate::sys::fd::native(self.file.as_raw_fd(), opcode, &input, output))?
    }

    pub fn open_file(&self, path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let path = beneath(path)?;
        let creating = opts.wire_flags()?
            & (ask_io::fs::OPEN_CREATE | ask_io::fs::OPEN_TRUNCATE | ask_io::fs::OPEN_APPEND)
            != 0;
        let rights = if creating { ask_io::fd::rights::WRITE } else { ask_io::fd::rights::READ };
        File::open_native(self.provider_token, Some(self.token(rights)?), path, opts)
    }

    pub fn open_dir(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self> {
        Dir::from_file(self.open_file(path, opts)?, self.provider_token)
    }

    pub fn self_metadata(&self) -> io::Result<FileAttr> {
        self.file.file_attr()
    }

    pub fn as_fd(&self) -> crate::os::fd::BorrowedFd<'_> {
        crate::os::fd::AsFd::as_fd(&self.file)
    }

    fn relative_metadata(&self, path: &Path, nofollow: bool) -> io::Result<FileAttr> {
        let path = beneath(path)?;
        let mut reply = [0; ask_io::fs::METADATA_LEN];
        let length = self.relative_native(
            ask_io::fs::OP_METADATA,
            path,
            |request: &mut [u8; ask_io::fs::FS_METADATA_PATH_REQUEST_LEN], window| {
                ask_io::fs::encode_fs_metadata_relative_request(request, window, path, ask_io::fs::MetadataLookup::from_nofollow(nofollow))
                    .is_some()
            },
            &mut reply,
        )?;
        reply
            .get(..length)
            .and_then(ask_io::fs::decode_fs_metadata)
            .map(FileAttr::from_metadata)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
    }

    pub fn metadata(&self, path: &Path) -> io::Result<FileAttr> {
        self.relative_metadata(path, false)
    }

    pub fn symlink_metadata(&self, path: &Path) -> io::Result<FileAttr> {
        self.relative_metadata(path, true)
    }

    /// The names directly beneath this directory, in provider cookie order.
    pub fn entry_names(&self) -> io::Result<crate::vec::Vec<OsString>> {
        let mut names = crate::vec::Vec::new();
        let mut cookie = ask_io::fs::DirCookie::default();
        loop {
            // Native replies exclude the Channel header; the owner stages paths separately.
            let mut reply = [0; ask_abi::ipc::MAX_MSG_LEN - ask_abi::ipc::HEADER_LEN];
            let length = self.relative_native(
                ask_io::fs::OP_READ_DIRECTORY,
                &[],
                |request: &mut [u8; ask_io::fs::FS_DIRECTORY_REQUEST_LEN], window| {
                    ask_io::fs::encode_fs_directory_request(request, window, cookie, &[]).is_some()
                },
                &mut reply,
            )?;
            // The attachment form ends with an empty successful reply.
            if length == 0 {
                return Ok(names);
            }
            let (next, _, _, name) = reply
                .get(..length)
                .and_then(ask_io::fs::decode_fs_directory_reply)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            if next.generation == 0 || next == cookie {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            cookie = next;
            let name = crate::str::from_utf8(name)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            names.push(OsString::from(name));
        }
    }

    /// Without `follow`, a final symlink is refused rather than followed, so
    /// a traversal that stat'ed a non-symlink cannot be redirected by a swap.
    pub fn set_permissions_at(
        &self,
        path: &Path,
        perm: FilePermissions,
        follow: bool,
    ) -> io::Result<()> {
        let path = beneath(path)?;
        self.relative_native(
            ask_io::fs::OP_CHMOD,
            path,
            |request: &mut [u8; ask_io::fs::FS_CHMOD_PATH_REQUEST_LEN], window| {
                ask_io::fs::encode_fs_chmod_relative_request(request, window, path, perm.mode, !follow)
                    .is_some()
            },
            &mut [],
        )
        .map(|_| ())
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

    /// `DirBuilder` exposes no mode on this target, so request the POSIX
    /// directory mode with the process umask applied.
    pub fn mkdir(&self, path: &Path) -> io::Result<()> {
        let (provider_token, relative) = resolve_fs_path(path)?;
        let directory_mode = masked_mode(creation_mode::DIRECTORY)?;
        metadata_unit(
            provider_token,
            ask_io::fs::OP_MKDIR,
            |request: &mut [u8; ask_io::fs::FS_MKDIR_REQUEST_LEN], window| {
                ask_io::fs::encode_fs_mkdir_request(
                    request,
                    window,
                    directory_mode,
                    relative.as_bytes(),
                )
                .is_some()
            },
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
    /// For the view root: which mounts are bound, listed in `MOUNTS` order.
    mounts: Option<[bool; ask_abi::view::BINDINGS]>,
    next_mount: usize,
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
        if let Some(bound) = self.mounts {
            while let (Some(&present), Some(&mount)) =
                (bound.get(self.next_mount), ask_io::view::MOUNTS.get(self.next_mount))
            {
                self.next_mount += 1;
                if present {
                    return Some(Ok(DirEntry {
                        path: self.root.join(mount.name()),
                        name: OsString::from(mount.name()),
                        kind: ask_io::fs::NodeKind::Directory as u8,
                    }));
                }
            }
            self.exhausted = true;
            return None;
        }
        let cookie = self.cookie;
        let relative = self.relative.as_bytes();
        let completion = match path_exchange(
            self.provider_token,
            ask_io::fs::OP_READ_DIRECTORY,
            |request: &mut [u8; ask_io::fs::FS_DIRECTORY_REQUEST_LEN], window| {
                ask_io::fs::encode_fs_directory_request(request, window, cookie, relative).is_some()
            },
        ) {
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
        if let Err(error) = map_fs_result(completion.result, "fs: readdir failed") {
            self.exhausted = true;
            return Some(Err(error));
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
            kind,
        }))
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

    /// A full attribute set requires its own path-addressed metadata query.
    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat_path(&self.path, true)
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(FileType { kind: self.kind })
    }
}

pub fn readdir(path: &Path) -> io::Result<ReadDir> {
    let mut absolute = [0u8; ask_io::fs::PATH_MAX];
    let absolute = absolute_view_path(path, &mut absolute)?;
    if absolute == ask_io::path::ROOT {
        let view = startup_view()?;
        return Ok(ReadDir {
            provider_token: 0,
            relative: heapless_path::PathBuf::new(),
            root: path.to_path_buf(),
            cookie: ask_io::fs::DirCookie::default(),
            exhausted: false,
            mounts: Some(ask_io::view::MOUNTS.map(|mount| view.binding_present(mount))),
            next_mount: 0,
        });
    }
    let (provider_token, relative) = resolve_absolute(absolute)?;
    Ok(ReadDir {
        provider_token,
        relative,
        root: path.to_path_buf(),
        cookie: ask_io::fs::DirCookie::default(),
        exhausted: false,
        mounts: None,
        next_mount: 0,
    })
}

fn remove_node(path: &Path, kind: ask_io::fs::RemoveKind, message: &'static str) -> io::Result<()> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    metadata_unit(
        provider_token,
        ask_io::fs::OP_UNLINK,
        |request: &mut [u8; ask_io::fs::FS_UNLINK_REQUEST_LEN], window| {
            ask_io::fs::encode_fs_unlink_request(request, window, relative.as_bytes(), kind)
                .is_some()
        },
        message,
    )
}

pub fn unlink(path: &Path) -> io::Result<()> {
    remove_node(path, ask_io::fs::RemoveKind::NonDirectory, "fs: unlink failed")
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let (old_provider, old_relative) = resolve_fs_path(old)?;
    let (new_provider, new_relative) = resolve_fs_path(new)?;
    // `FS_OP_RENAME` is one provider's own directory operation; a move across
    // two mounts would need a copy plus unlink, which is the caller's policy
    // to choose, not this PAL's.
    if old_provider != new_provider {
        return Err(io::const_error!(io::ErrorKind::CrossesDevices, "fs: rename across providers"));
    }
    two_path_unit(
        old_provider,
        ask_io::fs::OP_RENAME,
        old_relative.as_bytes(),
        new_relative.as_bytes(),
        "fs: rename failed",
    )
}

pub fn rmdir(path: &Path) -> io::Result<()> {
    remove_node(path, ask_io::fs::RemoveKind::Directory, "fs: rmdir failed")
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

/// Returns the stored target bytes; an absolute target was stored provider-relative.
pub fn readlink(path: &Path) -> io::Result<PathBuf> {
    path_reply(path, ask_io::fs::OP_READLINK,
        |request: &mut [u8; ask_io::fs::FS_PATH_REQUEST_LEN], window, path| {
            ask_io::fs::encode_fs_path_request(request, window, path).is_some()
        })
}

fn path_reply<const N: usize>(path: &Path, opcode: u32,
    encode: impl FnOnce(&mut [u8; N], &mut [u8], &[u8]) -> bool,
) -> io::Result<PathBuf> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    let mut channel =
        SyncChannel::create_leased(provider_token, ask_io::fs::path_window::CHANNEL_PAGES)
            .map_err(|_| io::const_error!(io::ErrorKind::NotFound, "fs provider unreachable"))?;
    let mut request = [0u8; N];
    let window = channel
        .shared_region_mut(ask_io::fs::path_window::OFFSET, ask_io::fs::path_window::LEN)
        .ok_or_else(unsupported_err)?;
    if !encode(&mut request, window, relative.as_bytes()) {
        return Err(io::const_error!(io::ErrorKind::InvalidFilename, "fs: path too long"));
    }
    let completion = channel.call(opcode, &request)?;
    map_fs_result(completion.result, "fs: path lookup failed")?;
    let window = channel
        .shared_region_mut(ask_io::fs::path_window::OFFSET, ask_io::fs::path_window::LEN)
        .ok_or_else(unsupported_err)?;
    let target = ask_io::fs::decode_fs_path_reply(completion.payload(), window)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let target = crate::str::from_utf8(target)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    Ok(PathBuf::from(target))
}

/// A relative target is stored verbatim; an absolute one must name the
/// link's own provider and is stored provider-relative, as askposix does.
pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    let (provider_token, link_relative) = resolve_fs_path(link)?;
    let target = if original.is_absolute() {
        let (target_provider, target_relative) = resolve_fs_path(original)?;
        if target_provider != provider_token {
            return Err(io::const_error!(
                io::ErrorKind::CrossesDevices,
                "fs: symlink target on another provider",
            ));
        }
        target_relative
    } else {
        let mut target = heapless_path::PathBuf::new();
        target.push(original.to_str().ok_or_else(unsupported_err)?.as_bytes())?;
        target
    };
    two_path_unit(
        provider_token,
        ask_io::fs::OP_SYMLINK,
        target.as_bytes(),
        link_relative.as_bytes(),
        "fs: symlink failed",
    )
}

pub fn link(original: &Path, link: &Path) -> io::Result<()> {
    let (provider_token, original_relative) = resolve_fs_path(original)?;
    let (link_provider, link_relative) = resolve_fs_path(link)?;
    if link_provider != provider_token {
        return Err(io::const_error!(io::ErrorKind::CrossesDevices, "fs: link across providers"));
    }
    two_path_unit(
        provider_token,
        ask_io::fs::OP_LINK,
        original_relative.as_bytes(),
        link_relative.as_bytes(),
        "fs: link failed",
    )
}

fn two_path_unit(
    provider_token: u32,
    op: u32,
    first: &[u8],
    second: &[u8],
    failure: &'static str,
) -> io::Result<()> {
    metadata_unit(
        provider_token,
        op,
        |request: &mut [u8; ask_io::fs::FS_TWO_PATH_REQUEST_LEN], window| {
            ask_io::fs::encode_fs_two_path_request(request, window, first, second).is_some()
        },
        failure,
    )
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    stat_path(path, false)
}

pub fn lstat(path: &Path) -> io::Result<FileAttr> {
    stat_path(path, true)
}

pub fn set_perm(path: &Path, perm: FilePermissions) -> io::Result<()> {
    let (provider_token, relative) = resolve_fs_path(path)?;
    metadata_unit(
        provider_token,
        ask_io::fs::OP_CHMOD,
        |request: &mut [u8; ask_io::fs::FS_CHMOD_PATH_REQUEST_LEN], window| {
            ask_io::fs::encode_fs_chmod_path_request(request, window, relative.as_bytes(), perm.mode)
                .is_some()
        },
        "fs: chmod failed",
    )
}

pub use set_perm as set_perm_nofollow;

pub fn set_times(path: &Path, times: FileTimes) -> io::Result<()> {
    set_path_times(path, times, false)
}

pub fn set_times_nofollow(path: &Path, times: FileTimes) -> io::Result<()> {
    set_path_times(path, times, true)
}

/// The provider resolves the path and stores the requested fields in one
/// transaction, so an omitted field is never read back and rewritten here.
fn set_path_times(path: &Path, times: FileTimes, nofollow: bool) -> io::Result<()> {
    let update = times.to_update()?;
    let (provider_token, relative) = resolve_fs_path(path)?;
    let completion = path_exchange(
        provider_token,
        ask_io::fs::OP_UTIMENS,
        |request: &mut [u8; ask_io::fs::FS_UTIMENS_PATH_REQUEST_LEN], window| {
            ask_io::fs::encode_fs_utimens_path_request(
                request,
                window,
                relative.as_bytes(),
                nofollow,
                update,
            )
            .is_some()
        },
    )?;
    map_fs_result(completion.result, "fs: set times failed")?;
    Ok(())
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let mut absolute = [0; ask_io::fs::PATH_MAX];
    let absolute = absolute_view_path(path, &mut absolute)?;
    if absolute == ask_io::path::ROOT { return Ok(PathBuf::from(absolute)); }
    let view = startup_view()?;
    let binding = view.resolve(absolute, ask_io::view::Operation::Read).map_err(view_error)?;
    let resolved = path_reply(Path::new(absolute), ask_io::fs::OP_METADATA,
        |request: &mut [u8; ask_io::fs::FS_METADATA_PATH_REQUEST_LEN], window, path| {
            ask_io::fs::encode_fs_metadata_path_request(request, window, path,
                ask_io::fs::MetadataLookup::Canonical).is_some()
        })?;
    let resolved = resolved.to_str().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut output = [0; ask_io::fs::PATH_MAX];
    Ok(PathBuf::from(binding.decode_path(resolved, &mut output).map_err(view_error)?))
}
