#![stable(feature = "rust1", since = "1.0.0")]

use crate::io;
use crate::sys::FromInner;

#[stable(feature = "rust1", since = "1.0.0")]
pub type RawFd = i32;
#[stable(feature = "rust1", since = "1.0.0")]
pub trait AsRawFd {
    #[stable(feature = "rust1", since = "1.0.0")]
    fn as_raw_fd(&self) -> RawFd;
}
#[stable(feature = "rust1", since = "1.0.0")]
pub trait IntoRawFd {
    #[stable(feature = "rust1", since = "1.0.0")]
    fn into_raw_fd(self) -> RawFd;
}
#[stable(feature = "rust1", since = "1.0.0")]
pub trait FromRawFd {
    /// The alias must be live and uniquely owned by the caller.
    #[stable(feature = "rust1", since = "1.0.0")]
    unsafe fn from_raw_fd(fd: RawFd) -> Self;
}

#[stable(feature = "raw_fd_reflexive_traits", since = "1.48.0")]
impl AsRawFd for RawFd {
    fn as_raw_fd(&self) -> RawFd {
        *self
    }
}
#[stable(feature = "raw_fd_reflexive_traits", since = "1.48.0")]
impl IntoRawFd for RawFd {
    fn into_raw_fd(self) -> RawFd {
        self
    }
}
#[stable(feature = "raw_fd_reflexive_traits", since = "1.48.0")]
impl FromRawFd for RawFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        fd
    }
}

#[stable(feature = "rust1", since = "1.0.0")]
impl AsRawFd for crate::fs::File {
    fn as_raw_fd(&self) -> RawFd {
        crate::sys::AsInner::as_inner(self).as_raw_fd()
    }
}
#[stable(feature = "from_raw_os", since = "1.1.0")]
impl FromRawFd for crate::fs::File {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self::from(unsafe { super::OwnedFd::from_raw_fd(fd) })
    }
}
#[stable(feature = "into_raw_os", since = "1.4.0")]
impl IntoRawFd for crate::fs::File {
    fn into_raw_fd(self) -> RawFd {
        super::OwnedFd::from(self).into_raw_fd()
    }
}

#[stable(feature = "asrawfd_ptrs", since = "1.63.0")]
impl<T: AsRawFd> AsRawFd for crate::sync::Arc<T> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        (**self).as_raw_fd()
    }
}

#[stable(feature = "asfd_rc", since = "1.69.0")]
impl<T: AsRawFd> AsRawFd for crate::rc::Rc<T> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        (**self).as_raw_fd()
    }
}

#[unstable(feature = "unique_rc_arc", issue = "112566")]
impl<T: AsRawFd + ?Sized> AsRawFd for crate::rc::UniqueRc<T> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        (**self).as_raw_fd()
    }
}

#[stable(feature = "asrawfd_ptrs", since = "1.63.0")]
impl<T: AsRawFd, A: crate::alloc::Allocator> AsRawFd for Box<T, A> {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        (**self).as_raw_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl AsRawFd for io::PipeReader {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl FromRawFd for io::PipeReader {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self::from_inner(unsafe { FromRawFd::from_raw_fd(raw_fd) })
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl IntoRawFd for io::PipeReader {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl AsRawFd for io::PipeWriter {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl FromRawFd for io::PipeWriter {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self::from_inner(unsafe { FromRawFd::from_raw_fd(raw_fd) })
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl IntoRawFd for io::PipeWriter {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}
