//! Native sockets use the process resident descriptor owner.

use askrust::net::{Endpoint, NO_TIMEOUT, family};

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{
    Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs,
};
use crate::os::fd::FromRawFd;
use crate::sync::Mutex;
use crate::sys::fd::FileDesc;
use crate::sys::pal::{ask_error, unsupported_err};
use crate::sys::unsupported;
use crate::time::Duration;

fn first_addr<A: ToSocketAddrs>(addr: A) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?.next().ok_or(io::Error::NO_ADDRESSES)
}
fn to_endpoint(addr: SocketAddr) -> io::Result<Endpoint> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut address = [0; 16];
            address[..4].copy_from_slice(&v4.ip().octets());
            Ok(Endpoint { family: family::IPV4, port: v4.port(), address })
        }
        SocketAddr::V6(v6) if v6.scope_id() == 0 && v6.flowinfo() == 0 => {
            Ok(Endpoint { family: family::IPV6, port: v6.port(), address: v6.ip().octets() })
        }
        _ => Err(unsupported_err()),
    }
}
fn socket_addr(endpoint: Endpoint) -> io::Result<SocketAddr> {
    match endpoint.family {
        family::IPV4 => {
            let bytes: [u8; 4] = endpoint
                .address
                .get(..4)
                .and_then(|v| v.try_into().ok())
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(bytes), endpoint.port)))
        }
        family::IPV6 => Ok(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(endpoint.address),
            endpoint.port,
            0,
            0,
        ))),
        _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
    }
}
fn millis(timeout: Option<Duration>) -> u64 {
    timeout.map_or(NO_TIMEOUT, |value| u64::try_from(value.as_millis()).unwrap_or(NO_TIMEOUT))
}
fn socket(fd: i32) -> FileDesc {
    // SAFETY: the runtime returned a newly bound socket descriptor owned by the caller.
    unsafe { FileDesc::from_raw_fd(fd) }
}
fn address(socket: &FileDesc, peer: bool) -> io::Result<SocketAddr> {
    socket_addr(askrust::net::address(socket.as_raw_fd(), peer).map_err(ask_error)?)
}
fn set_option(socket: &FileDesc, option: u32, value: bool) -> io::Result<()> {
    askrust::net::set_option(socket.as_raw_fd(), option, value).map_err(ask_error)
}
fn take_error(socket: &FileDesc) -> io::Result<Option<io::Error>> {
    let code = askrust::net::take_error(socket.as_raw_fd()).map_err(ask_error)?;
    Ok((code != 0).then(|| io::Error::from_raw_os_error(i32::from(code))))
}
fn recv(socket: &FileDesc, buf: &mut [u8], timeout: Option<Duration>, datagram: bool) -> io::Result<(usize, Endpoint)> {
    askrust::net::recv(socket.as_raw_fd(), buf, millis(timeout), datagram).map_err(ask_error)
}
fn send(socket: &FileDesc, buf: &[u8], timeout: Option<Duration>, destination: Option<SocketAddr>) -> io::Result<usize> {
    let destination = destination.map(to_endpoint).transpose()?;
    askrust::net::send(socket.as_raw_fd(), buf, millis(timeout), destination.as_ref()).map_err(ask_error)
}
fn validate_timeout(timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|value| value.is_zero()) {
        Err(io::Error::from(io::ErrorKind::InvalidInput))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct TcpStream {
    socket: FileDesc,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}
impl TcpStream {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        Self::connect_timeout(&first_addr(addr)?, Duration::MAX)
    }

    pub fn connect_timeout(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        validate_timeout(Some(timeout))?;
        let endpoint = to_endpoint(*addr)?;
        let timeout = u64::try_from(timeout.as_millis()).unwrap_or(NO_TIMEOUT);
        let socket = socket(askrust::net::tcp_connect(&endpoint, timeout).map_err(ask_error)?);
        Ok(Self { socket, read_timeout: Mutex::new(None), write_timeout: Mutex::new(None) })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.write_timeout.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        recv(&self.socket, buf, self.read_timeout()?, false).map(|(count, _)| count)
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|buf| self.read(buf), cursor)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        send(&self.socket, buf, self.write_timeout()?, None)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        address(&self.socket, true)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        address(&self.socket, false)
    }

    pub fn shutdown(&self, shutdown: Shutdown) -> io::Result<()> {
        let how = match shutdown {
            Shutdown::Read => askrust::net::shutdown::READ,
            Shutdown::Write => askrust::net::shutdown::WRITE,
            Shutdown::Both => askrust::net::shutdown::BOTH,
        };
        askrust::net::shutdown(self.socket.as_raw_fd(), how).map_err(ask_error)
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        Ok(Self {
            socket: self.socket.duplicate()?,
            read_timeout: Mutex::new(self.read_timeout()?),
            write_timeout: Mutex::new(self.write_timeout()?),
        })
    }

    pub fn set_linger(&self, _timeout: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn set_keepalive(&self, _keepalive: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        set_option(&self.socket, askrust::net::option::NODELAY, nodelay)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        askrust::net::option(self.socket.as_raw_fd(), askrust::net::option::NODELAY).map_err(ask_error)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_hop_limit_v6(&self, _: u8) -> io::Result<()> {
        unsupported()
    }

    pub fn hop_limit_v6(&self) -> io::Result<u8> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        take_error(&self.socket)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_option(&self.socket, askrust::net::option::NONBLOCKING, nonblocking)
    }
}

#[derive(Debug)]
pub struct TcpListener {
    socket: FileDesc,
}
impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let endpoint = to_endpoint(first_addr(addr)?)?;
        Ok(Self { socket: socket(askrust::net::tcp_listen(&endpoint).map_err(ask_error)?) })
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        address(&self.socket, false)
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let (fd, peer) = askrust::net::tcp_accept(self.socket.as_raw_fd()).map_err(ask_error)?;
        let stream = TcpStream {
            socket: socket(fd),
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        };
        Ok((stream, socket_addr(peer)?))
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        Ok(Self { socket: self.socket.duplicate()? })
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_hop_limit_v6(&self, _: u8) -> io::Result<()> {
        unsupported()
    }

    pub fn hop_limit_v6(&self) -> io::Result<u8> {
        unsupported()
    }

    pub fn set_only_v6(&self, _only_v6: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        take_error(&self.socket)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_option(&self.socket, askrust::net::option::NONBLOCKING, nonblocking)
    }
}

#[derive(Debug)]
pub struct UdpSocket {
    socket: FileDesc,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}
impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let endpoint = to_endpoint(first_addr(addr)?)?;
        let socket = socket(askrust::net::udp_bind(&endpoint).map_err(ask_error)?);
        Ok(Self { socket, read_timeout: Mutex::new(None), write_timeout: Mutex::new(None) })
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        address(&self.socket, true)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        address(&self.socket, false)
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (count, peer) = recv(&self.socket, buf, self.read_timeout()?, true)?;
        Ok((count, socket_addr(peer)?))
    }

    pub fn peek_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        send(&self.socket, buf, self.write_timeout()?, Some(*addr))
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        Ok(Self {
            socket: self.socket.duplicate()?,
            read_timeout: Mutex::new(self.read_timeout()?),
            write_timeout: Mutex::new(self.write_timeout()?),
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(*self.write_timeout.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn set_broadcast(&self, _broadcast: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn set_multicast_loop_v4(&self, _val: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_multicast_ttl_v4(&self, _val: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_multicast_hop_limit_v6(&self, _: u8) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_hop_limit_v6(&self) -> io::Result<u8> {
        unsupported()
    }

    pub fn set_multicast_loop_v6(&self, _val: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn join_multicast_v4(&self, _addr: &Ipv4Addr, _iface: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn join_multicast_v6(&self, _addr: &Ipv6Addr, _iface: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v4(&self, _addr: &Ipv4Addr, _iface: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v6(&self, _addr: &Ipv6Addr, _iface: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_hop_limit_v6(&self, _: u8) -> io::Result<()> {
        unsupported()
    }

    pub fn hop_limit_v6(&self) -> io::Result<u8> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        take_error(&self.socket)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_option(&self.socket, askrust::net::option::NONBLOCKING, nonblocking)
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        recv(&self.socket, buf, self.read_timeout()?, false).map(|(count, _)| count)
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        send(&self.socket, buf, self.write_timeout()?, None)
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let endpoint = to_endpoint(first_addr(addr)?)?;
        askrust::net::connect(self.socket.as_raw_fd(), &endpoint).map_err(ask_error)
    }
}

pub struct LookupHost {
    addresses: [SocketAddr; askrust::net::MAX_ADDRESSES],
    count: usize,
    index: usize,
}

impl LookupHost {
    fn one(addr: SocketAddr) -> LookupHost {
        LookupHost { addresses: [addr; askrust::net::MAX_ADDRESSES], count: 1, index: 0 }
    }
}

impl Iterator for LookupHost {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        let addr = self.addresses.get(self.index).copied().filter(|_| self.index < self.count)?;
        self.index += 1;
        Some(addr)
    }
}

impl TryFrom<&str> for LookupHost {
    type Error = io::Error;

    fn try_from(host_port: &str) -> io::Result<LookupHost> {
        if let Ok(addr) = host_port.parse::<SocketAddr>() {
            return Ok(LookupHost::one(addr));
        }
        let (host, port) = host_port
            .rsplit_once(':')
            .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "missing port"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| io::const_error!(io::ErrorKind::InvalidInput, "invalid port"))?;
        LookupHost::try_from((host, port))
    }
}

impl<'a> TryFrom<(&'a str, u16)> for LookupHost {
    type Error = io::Error;

    fn try_from((host, port): (&'a str, u16)) -> io::Result<LookupHost> {
        let mut endpoints = [Endpoint::default(); askrust::net::MAX_ADDRESSES];
        let count = askrust::net::lookup(host, port, &mut endpoints).map_err(ask_error)?;
        if count == 0 {
            return Err(io::Error::NO_ADDRESSES);
        }
        let unspecified = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
        let mut addresses = [unspecified; askrust::net::MAX_ADDRESSES];
        for (slot, endpoint) in addresses.iter_mut().zip(endpoints.iter().take(count)) {
            *slot = socket_addr(*endpoint)?;
        }
        Ok(LookupHost { addresses, count, index: 0 })
    }
}

pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    LookupHost::try_from((host, port))
}
