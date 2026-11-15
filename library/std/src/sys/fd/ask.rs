use crate::io;
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

#[derive(Debug)]
pub struct FileDesc(OwnedFd);

impl FileDesc {
    pub fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
    pub fn duplicate(&self) -> io::Result<Self> {
        self.try_clone()
    }
    pub fn read(&self, output: &mut [u8]) -> io::Result<usize> {
        askrust::fd::read(self.as_raw_fd(), output).map_err(crate::sys::pal::ask_error)
    }
    pub fn write(&self, input: &[u8]) -> io::Result<usize> {
        askrust::fd::write(self.as_raw_fd(), input).map_err(crate::sys::pal::ask_error)
    }
    pub fn flush(&self) -> io::Result<()> {
        askrust::fd::flush(self.as_raw_fd()).map_err(crate::sys::pal::ask_error)
    }
    pub fn read_buf(&self, cursor: io::BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|buf| self.read(buf), cursor)
    }
    pub fn read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        io::default_read_vectored(|buf| self.read(buf), bufs)
    }
    pub fn write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        io::default_write_vectored(|buf| self.write(buf), bufs)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
}
impl AsRawFd for FileDesc {
    fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}
impl AsFd for FileDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl IntoRawFd for FileDesc {
    fn into_raw_fd(self) -> i32 {
        self.0.into_raw_fd()
    }
}
impl FromRawFd for FileDesc {
    unsafe fn from_raw_fd(fd: i32) -> Self {
        Self(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
impl crate::sys::AsInner<OwnedFd> for FileDesc {
    fn as_inner(&self) -> &OwnedFd {
        &self.0
    }
}
impl crate::sys::IntoInner<OwnedFd> for FileDesc {
    fn into_inner(self) -> OwnedFd {
        self.0
    }
}
impl crate::sys::FromInner<OwnedFd> for FileDesc {
    fn from_inner(input: OwnedFd) -> Self {
        Self(input)
    }
}

pub(crate) fn close(number: i32) {
    askrust::fd::close(number)
}

pub(crate) fn duplicate(number: i32) -> io::Result<OwnedFd> {
    let raw = askrust::fd::duplicate(number).map_err(crate::sys::pal::ask_error)?;
    // SAFETY: the runtime returned a newly bound descriptor owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

pub(crate) fn is_terminal(number: i32) -> bool {
    askrust::fd::is_terminal(number)
}
