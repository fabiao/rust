#![stable(feature = "io_safety", since = "1.63.0")]

use super::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use crate::marker::PhantomData;
use crate::mem::ManuallyDrop;
use crate::{fmt, io};
type ValidRawFd = core::num::niche_types::NotAllOnes<RawFd>;

#[repr(transparent)]
#[rustc_nonnull_optimization_guaranteed]
#[stable(feature = "io_safety", since = "1.63.0")]
pub struct OwnedFd {
    fd: ValidRawFd,
}

#[derive(Clone, Copy)]
#[repr(transparent)]
#[rustc_nonnull_optimization_guaranteed]
#[stable(feature = "io_safety", since = "1.63.0")]
pub struct BorrowedFd<'fd> {
    fd: ValidRawFd,
    marker: PhantomData<&'fd OwnedFd>,
}

#[stable(feature = "io_safety", since = "1.63.0")]
pub trait AsFd {
    #[stable(feature = "io_safety", since = "1.63.0")]
    fn as_fd(&self) -> BorrowedFd<'_>;
}

impl BorrowedFd<'_> {
    #[rustc_const_stable(feature = "io_safety", since = "1.63.0")]
    #[stable(feature = "io_safety", since = "1.63.0")]
    pub const unsafe fn borrow_raw(fd: RawFd) -> Self {
        Self { fd: ValidRawFd::new(fd).expect("fd != -1"), marker: PhantomData }
    }
    #[stable(feature = "io_safety", since = "1.63.0")]
    pub fn try_clone_to_owned(&self) -> io::Result<OwnedFd> {
        crate::sys::fd::duplicate(self.as_raw_fd())
    }
}
impl OwnedFd {
    #[stable(feature = "io_safety", since = "1.63.0")]
    pub fn try_clone(&self) -> io::Result<Self> {
        self.as_fd().try_clone_to_owned()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl Drop for OwnedFd {
    fn drop(&mut self) {
        crate::sys::fd::close(self.as_raw_fd());
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl FromRawFd for OwnedFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd: ValidRawFd::new(fd).expect("fd != -1") }
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl IntoRawFd for OwnedFd {
    fn into_raw_fd(self) -> RawFd {
        ManuallyDrop::new(self).fd.as_inner()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_inner()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl AsRawFd for BorrowedFd<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_inner()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for OwnedFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for BorrowedFd<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        *self
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl<T: AsFd + ?Sized> AsFd for &T {
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl<T: AsFd + ?Sized> AsFd for &mut T {
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl fmt::Debug for OwnedFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedFd").field("fd", &self.as_raw_fd()).finish()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl fmt::Debug for BorrowedFd<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BorrowedFd").field("fd", &self.as_raw_fd()).finish()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
impl AsFd for crate::fs::File {
    fn as_fd(&self) -> BorrowedFd<'_> {
        crate::sys::AsInner::as_inner(self).as_fd()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl From<crate::fs::File> for OwnedFd {
    fn from(file: crate::fs::File) -> Self {
        use crate::sys::IntoInner;
        file.into_inner().into_inner().into_inner()
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl From<OwnedFd> for crate::fs::File {
    fn from(fd: OwnedFd) -> Self {
        use crate::sys::FromInner;
        Self::from_inner(FromInner::from_inner(FromInner::from_inner(fd)))
    }
}
#[stable(feature = "io_safety", since = "1.63.0")]
impl From<OwnedFd> for crate::process::Stdio {
    fn from(fd: OwnedFd) -> Self {
        use crate::sys::FromInner;
        let fd = crate::sys::fd::FileDesc::from_inner(fd);
        Self::from_inner(crate::sys::process::Stdio::Fd(fd))
    }
}
#[stable(feature = "is_terminal", since = "1.70.0")]
impl io::IsTerminal for OwnedFd {
    fn is_terminal(&self) -> bool {
        crate::sys::io::is_terminal(self)
    }
}
#[stable(feature = "is_terminal", since = "1.70.0")]
impl io::IsTerminal for BorrowedFd<'_> {
    fn is_terminal(&self) -> bool {
        crate::sys::io::is_terminal(self)
    }
}

#[stable(feature = "asfd_ptrs", since = "1.64.0")]
impl<T: AsFd + ?Sized> AsFd for crate::sync::Arc<T> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}

#[stable(feature = "asfd_rc", since = "1.69.0")]
impl<T: AsFd + ?Sized> AsFd for crate::rc::Rc<T> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}

#[unstable(feature = "unique_rc_arc", issue = "112566")]
impl<T: AsFd + ?Sized> AsFd for crate::rc::UniqueRc<T> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}

#[stable(feature = "asfd_ptrs", since = "1.64.0")]
impl<T: AsFd + ?Sized, A: crate::alloc::Allocator> AsFd for Box<T, A> {
    #[inline]
    fn as_fd(&self) -> BorrowedFd<'_> {
        (**self).as_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl AsFd for io::PipeReader {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl From<io::PipeReader> for OwnedFd {
    fn from(pipe: io::PipeReader) -> Self {
        crate::sys::IntoInner::into_inner(pipe.0)
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl AsFd for io::PipeWriter {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl From<io::PipeWriter> for OwnedFd {
    fn from(pipe: io::PipeWriter) -> Self {
        crate::sys::IntoInner::into_inner(pipe.0)
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl From<OwnedFd> for io::PipeReader {
    fn from(owned_fd: OwnedFd) -> Self {
        Self(crate::sys::FromInner::from_inner(owned_fd))
    }
}

#[stable(feature = "anonymous_pipe", since = "1.87.0")]
impl From<OwnedFd> for io::PipeWriter {
    fn from(owned_fd: OwnedFd) -> Self {
        Self(crate::sys::FromInner::from_inner(owned_fd))
    }
}
