//! Buffered native streams with independently retained resident descriptors.
#![unstable(feature = "libstd_sys_internals", issue = "none")]

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, Read};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd};
use crate::sys::fd::{self, FileDesc};

mod policy {
    /// Requested FIFO capacity for the standard anonymous-pipe constructor.
    pub const CAPACITY: u64 = 64 * 1024;
}

pub struct Pipe {
    descriptor: FileDesc,
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    use ask_io::pipe::buffered;
    if crate::rt::ask_entry::execution_class() == ask_fd::execution_class::CLASS1 {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    let mut endpoint = ask_ipc::directory::connect(
        ask_io::directory::EndpointUse::Once,
        buffered::INTERFACE_NAME,
        buffered::INTERFACE_VERSION,
        buffered::facet::ALL,
        ask_ipc::sync::WaitHooks::RAW,
    )
    .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
    let provider =
        endpoint.provider_identity().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut channel =
        endpoint.create_channel(1).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    let deadline = ask_sys::get_monotonic_ms()
        .checked_add(ask_ipc::directory::policy::DEADLINE_MS)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let rights = ask_io::fd::rights::ACCESS | ask_io::fd::rights::TRANSFER;
    let mut bytes = [0; buffered::CREATE_LEN];
    let payload = buffered::Create { capacity: policy::CAPACITY, deadline_ms: deadline, rights }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let completion = fd::call(&mut channel, buffered::CREATE, payload, deadline)?;
    if completion.result != 0 {
        return Err(io::Error::from(io::ErrorKind::Other));
    }
    let (code, body) = ask_io::fd::lifecycle_status::decode_reply(
        completion.payload(),
        buffered::CREATE_REPLY_LEN,
    )
    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    if code != ask_io::fd::lifecycle_status::OK {
        return Err(fd::lifecycle_error(code));
    }
    let reply = buffered::CreateReply::decode(body)
        .filter(|reply| reply.capacity == policy::CAPACITY)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let read = fd::redeem(
        provider,
        reply.reader,
        ask_io::fd::backend::STREAM,
        ask_io::fd::rights::READ | ask_io::fd::rights::TRANSFER,
        deadline,
    )?;
    let write = fd::redeem(
        provider,
        reply.writer,
        ask_io::fd::backend::STREAM,
        ask_io::fd::rights::WRITE | ask_io::fd::rights::TRANSFER,
        deadline,
    )?;
    Ok((Pipe { descriptor: read }, Pipe { descriptor: write }))
}

impl Pipe {
    pub fn as_raw_fd(&self) -> i32 {
        self.descriptor.as_raw_fd()
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        self.descriptor.try_clone().map(|descriptor| Self { descriptor })
    }
    pub fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        self.descriptor.read(bytes)
    }
    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        self.descriptor.read_buf(cursor)
    }
    pub fn read_vectored(&self, bytes: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        self.descriptor.read_vectored(bytes)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.descriptor.write(bytes)
    }
    pub fn write_vectored(&self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        self.descriptor.write_vectored(bytes)
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
    pub fn read_to_end(&self, bytes: &mut Vec<u8>) -> io::Result<usize> {
        let mut reader = self;
        Read::read_to_end(&mut reader, bytes)
    }
}
impl Read for &Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        Pipe::read(self, bytes)
    }
}
impl AsRawFd for Pipe {
    fn as_raw_fd(&self) -> i32 {
        self.descriptor.as_raw_fd()
    }
}
impl AsFd for Pipe {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.descriptor.as_fd()
    }
}
impl IntoRawFd for Pipe {
    fn into_raw_fd(self) -> i32 {
        self.descriptor.into_raw_fd()
    }
}
impl FromRawFd for Pipe {
    unsafe fn from_raw_fd(fd: i32) -> Self {
        Self { descriptor: unsafe { FileDesc::from_raw_fd(fd) } }
    }
}
impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe").field("fd", &self.as_raw_fd()).finish()
    }
}

impl crate::sys::IntoInner<crate::os::fd::OwnedFd> for Pipe {
    fn into_inner(self) -> crate::os::fd::OwnedFd {
        crate::sys::IntoInner::into_inner(self.descriptor)
    }
}
impl crate::sys::FromInner<crate::os::fd::OwnedFd> for Pipe {
    fn from_inner(fd: crate::os::fd::OwnedFd) -> Self {
        Self { descriptor: crate::sys::FromInner::from_inner(fd) }
    }
}
