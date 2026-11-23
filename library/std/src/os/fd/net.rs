use crate::macros::rustdoc_skip_body;
use crate::os::fd::owned::{AsFd, BorrowedFd, OwnedFd, unsafe_impl_fd_asrefs};
use crate::os::fd::raw::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use crate::sys::{AsInner, FromInner, IntoInner};
use crate::{net, sys};

macro_rules! impl_as_raw_fd {
    ($($t:ident)*) => {$(
        #[stable(feature = "rust1", since = "1.0.0")]
        impl AsRawFd for net::$t {
            #[inline]
            fn as_raw_fd(&self) -> RawFd {
                self.as_inner().socket().as_raw_fd()
            }
        }
    )*};
}
impl_as_raw_fd! { TcpStream TcpListener UdpSocket }

macro_rules! impl_from_raw_fd {
    ($($t:ident)*) => {$(
        #[stable(feature = "from_raw_os", since = "1.1.0")]
        impl FromRawFd for net::$t {
            #[inline]
            unsafe fn from_raw_fd(fd: RawFd) -> net::$t {
                unsafe {
                    let socket = sys::net::Socket::from_inner(FromInner::from_inner(OwnedFd::from_raw_fd(fd)));
                    net::$t::from_inner(sys::net::$t::from_inner(socket))
                }
            }
        }
    )*};
}
impl_from_raw_fd! { TcpStream TcpListener UdpSocket }

macro_rules! impl_into_raw_fd {
    ($($t:ident)*) => {$(
        #[stable(feature = "into_raw_os", since = "1.4.0")]
        impl IntoRawFd for net::$t {
            #[inline]
            fn into_raw_fd(self) -> RawFd {
                self.into_inner().into_socket().into_inner().into_inner().into_raw_fd()
            }
        }
    )*};
}
impl_into_raw_fd! { TcpStream TcpListener UdpSocket }

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl AsFd for crate::net::TcpStream {
    #[inline]
    #[rustdoc_skip_body]
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_inner().socket().as_fd()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<crate::net::TcpStream> for OwnedFd {
    /// Takes ownership of a [`TcpStream`](crate::net::TcpStream)'s socket file descriptor.
    #[inline]
    #[rustdoc_skip_body]
    fn from(tcp_stream: crate::net::TcpStream) -> OwnedFd {
        tcp_stream.into_inner().into_socket().into_inner().into_inner()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<OwnedFd> for crate::net::TcpStream {
    #[inline]
    #[rustdoc_skip_body]
    fn from(owned_fd: OwnedFd) -> Self {
        Self::from_inner(FromInner::from_inner(FromInner::from_inner(FromInner::from_inner(
            owned_fd,
        ))))
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl AsFd for crate::net::TcpListener {
    #[inline]
    #[rustdoc_skip_body]
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_inner().socket().as_fd()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<crate::net::TcpListener> for OwnedFd {
    /// Takes ownership of a [`TcpListener`](crate::net::TcpListener)'s socket file descriptor.
    #[inline]
    #[rustdoc_skip_body]
    fn from(tcp_listener: crate::net::TcpListener) -> OwnedFd {
        tcp_listener.into_inner().into_socket().into_inner().into_inner()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<OwnedFd> for crate::net::TcpListener {
    #[inline]
    #[rustdoc_skip_body]
    fn from(owned_fd: OwnedFd) -> Self {
        Self::from_inner(FromInner::from_inner(FromInner::from_inner(FromInner::from_inner(
            owned_fd,
        ))))
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl AsFd for crate::net::UdpSocket {
    #[inline]
    #[rustdoc_skip_body]
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_inner().socket().as_fd()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<crate::net::UdpSocket> for OwnedFd {
    /// Takes ownership of a [`UdpSocket`](crate::net::UdpSocket)'s file descriptor.
    #[inline]
    #[rustdoc_skip_body]
    fn from(udp_socket: crate::net::UdpSocket) -> OwnedFd {
        udp_socket.into_inner().into_socket().into_inner().into_inner()
    }
}

#[stable(feature = "io_safety", since = "1.63.0")]
#[cfg(not(target_os = "trusty"))]
impl From<OwnedFd> for crate::net::UdpSocket {
    #[inline]
    #[rustdoc_skip_body]
    fn from(owned_fd: OwnedFd) -> Self {
        Self::from_inner(FromInner::from_inner(FromInner::from_inner(FromInner::from_inner(
            owned_fd,
        ))))
    }
}

unsafe_impl_fd_asrefs!(net::TcpStream, net::TcpListener, net::UdpSocket);
