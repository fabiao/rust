//! Buffered native streams with independently retained resident descriptors.
#![unstable(feature = "libstd_sys_internals", issue = "none")]

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, Read};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd};
use crate::sys::fd::FileDesc;

pub struct Pipe {
    descriptor: FileDesc,
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let (read, write) = askrust::pipe::create().map_err(crate::sys::pal::ask_error)?;
    // SAFETY: the runtime returns two newly bound descriptors owned by the caller.
    unsafe {
        Ok((
            Pipe { descriptor: FileDesc::from_raw_fd(read) },
            Pipe { descriptor: FileDesc::from_raw_fd(write) },
        ))
    }
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
