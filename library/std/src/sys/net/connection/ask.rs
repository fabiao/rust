//! `std::net` PAL for ask: blocking `TcpStream`/`TcpListener`/`UdpSocket`
//! bridging onto `netstack`'s `NET_OP_*` wire protocol (`ask_io::net`) over
//! one process-wide `SyncChannel` — `netstack` accepts exactly one client
//! channel for its entire process lifetime (docs/rust-toolchain.md), so
//! every socket handle in this process multiplexes over the same connection
//! rather than opening one channel per socket, unlike `sys/fs/ask.rs`.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{
    Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs,
};
use crate::sync::{Mutex, MutexGuard, OnceLock};
use crate::sys::channel::SyncChannel;
use crate::sys::pal::unsupported_err;
use crate::sys::unsupported;
use crate::time::Duration;

/// The process-wide channel to `netstack`, created lazily on first socket
/// use and shared by every `TcpStream`/`TcpListener`/`UdpSocket` handle in
/// this process — see the module doc comment.
static NET_CHANNEL: OnceLock<Mutex<SyncChannel>> = OnceLock::new();

fn channel() -> io::Result<MutexGuard<'static, SyncChannel>> {
    let cell = NET_CHANNEL.get_or_try_init(|| {
        SyncChannel::create_leased(ask_abi::APP_NET_TOKEN, ask_io::net::CHANNEL_PAGES)
            .map(Mutex::new)
            .map_err(|_| io::const_error!(io::ErrorKind::NotConnected, "netstack unreachable"))
    })?;
    Ok(cell.lock().unwrap_or_else(|e| e.into_inner()))
}

fn to_net_endpoint(addr: SocketAddr) -> io::Result<ask_io::net::NetEndpoint> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut address = [0u8; 16];
            address[..4].copy_from_slice(&v4.ip().octets());
            Ok(ask_io::net::NetEndpoint {
                family: ask_io::net::AF_IPV4,
                port: v4.port(),
                address,
            })
        }
        SocketAddr::V6(v6) => Ok(ask_io::net::NetEndpoint {
            family: ask_io::net::AF_IPV6,
            port: v6.port(),
            address: v6.ip().octets(),
        }),
    }
}

fn lookup_socket_addr(endpoint: ask_io::net::NetEndpoint) -> io::Result<SocketAddr> {
    match endpoint.family {
        ask_io::net::AF_IPV4 => {
            let octets: [u8; 4] = endpoint
                .address
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(unsupported_err)?;
            Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(octets), endpoint.port)))
        }
        ask_io::net::AF_IPV6 => Ok(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(endpoint.address),
            endpoint.port,
            0,
            0,
        ))),
        _ => Err(unsupported_err()),
    }
}

fn from_net_endpoint(endpoint: ask_io::net::NetEndpoint) -> io::Result<SocketAddr> {
    lookup_socket_addr(endpoint)
}

fn first_addr<A: ToSocketAddrs>(addr: A) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?.next().ok_or(io::Error::NO_ADDRESSES)
}

/// Block until a deferred op's completion arrives, respecting an optional
/// wall-clock deadline — `netstack`'s server-side defer already does the
/// waiting-without-spinning half (it re-wakes this channel's peer once an op
/// becomes ready, docs/rust-toolchain.md), so this only needs to keep
/// parking (or time out) rather than retry `RESULT_WOULD_BLOCK` itself.
fn call_with_timeout(
    guard: &mut SyncChannel,
    opcode: u32,
    payload: &[u8],
    timeout: Option<Duration>,
) -> io::Result<crate::sys::channel::Completion> {
    guard.call_timeout(opcode, payload, timeout)
}

fn socket_call(
    opcode: u32,
    payload: &[u8],
) -> io::Result<crate::sys::channel::Completion> {
    let mut guard = channel()?;
    call_with_timeout(&mut guard, opcode, payload, None)
}

fn netstack_error(result: i32) -> io::Error {
    match result {
        ask_io::net::RESULT_WOULD_BLOCK => {
            io::const_error!(io::ErrorKind::WouldBlock, "socket operation would block")
        }
        ask_io::net::RESULT_UNSUPPORTED => unsupported_err(),
        ask_io::net::RESULT_INVALID => {
            io::const_error!(io::ErrorKind::InvalidInput, "netstack rejected request")
        }
        _ => io::const_error!(io::ErrorKind::Other, "netstack operation failed"),
    }
}

fn set_bool_option(
    handle: u32,
    option: ask_io::net::SocketOption,
    value: bool,
) -> io::Result<()> {
    let mut encoded_value = [0u8; 1];
    let encoded_value = ask_io::net::encode_net_bool_option(&mut encoded_value, value);
    let mut request = [0u8; ask_io::net::OPTION_HEADER_LEN + 1];
    let payload = ask_io::net::encode_net_setopt_request(
        &mut request,
        handle,
        option,
        encoded_value,
    )
    .ok_or_else(unsupported_err)?;
    let completion = socket_call(ask_io::net::OP_SETOPT, payload)?;
    if completion.result < 0 {
        return Err(netstack_error(completion.result));
    }
    Ok(())
}

fn get_bool_option(handle: u32, option: ask_io::net::SocketOption) -> io::Result<bool> {
    let mut request = [0u8; ask_io::net::OPTION_HEADER_LEN];
    let payload = ask_io::net::encode_net_getopt_request(&mut request, handle, option);
    let completion = socket_call(ask_io::net::OP_GETOPT, payload)?;
    if completion.result < 0 {
        return Err(netstack_error(completion.result));
    }
    ask_io::net::decode_net_bool_option(completion.payload()).ok_or_else(unsupported_err)
}

fn open_socket(protocol: u8, family: u8) -> io::Result<u32> {
    if family != ask_io::net::AF_IPV4 && family != ask_io::net::AF_IPV6 {
        return Err(unsupported_err());
    }
    let mut request = [0u8; 2];
    let payload = ask_io::net::encode_net_socket_request(&mut request, family, protocol);
    let completion = socket_call(ask_io::net::OP_SOCKET, payload)?;
    if completion.result < 0 {
        return Err(io::const_error!(io::ErrorKind::Other, "netstack: socket failed"));
    }
    ask_io::net::decode_net_handle(completion.payload()).ok_or_else(unsupported_err)
}

fn close_handle(handle: u32) {
    let mut request = [0u8; 4];
    let payload = ask_io::net::encode_net_handle(&mut request, handle);
    let _ = socket_call(ask_io::net::OP_CLOSE, payload);
}

#[derive(Debug)]
pub struct TcpStream {
    handle: u32,
    peer: SocketAddr,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

impl TcpStream {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        Self::connect_timeout(&first_addr(addr)?, Duration::MAX)
    }

    pub fn connect_timeout(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        let endpoint = to_net_endpoint(*addr)?;
        let handle = open_socket(ask_io::net::PROTO_TCP, endpoint.family)?;
        let mut request = [0u8; 23];
        let payload = ask_io::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        let timeout = if timeout == Duration::MAX { None } else { Some(timeout) };
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_CONNECT, payload, timeout)?;
        drop(guard);
        if completion.result < 0 {
            close_handle(handle);
            return Err(io::const_error!(io::ErrorKind::ConnectionRefused, "netstack: connect failed"));
        }
        Ok(TcpStream {
            handle,
            peer: *addr,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
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
        let timeout = *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let want = (buf.len() as u32).min(ask_io::net::DATA_LEN);
        let buffer = ask_io::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_io::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_RECV, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
            .ok_or_else(unsupported_err)?;
        buf[..n].copy_from_slice(data);
        Ok(n)
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
        let timeout = *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let n = (buf.len() as u32).min(ask_io::net::DATA_LEN) as usize;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_io::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_io::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_SEND, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        Ok(completion.result as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        unsupported()
    }

    pub fn shutdown(&self, shutdown: Shutdown) -> io::Result<()> {
        let direction = match shutdown {
            Shutdown::Read => ask_io::net::SHUTDOWN_READ,
            Shutdown::Write => ask_io::net::SHUTDOWN_WRITE,
            Shutdown::Both => ask_io::net::SHUTDOWN_BOTH,
        };
        let mut request = [0u8; 8];
        let payload =
            ask_io::net::encode_net_handle_value(&mut request, self.handle, direction as u32);
        let completion = socket_call(ask_io::net::OP_SHUTDOWN, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: shutdown failed"));
        }
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        unsupported()
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
        set_bool_option(self.handle, ask_io::net::SocketOption::NoDelay, nodelay)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        get_bool_option(self.handle, ask_io::net::SocketOption::NoDelay)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_bool_option(
            self.handle,
            ask_io::net::SocketOption::Nonblocking,
            nonblocking,
        )
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        close_handle(self.handle);
    }
}

#[derive(Debug)]
pub struct TcpListener {
    handle: u32,
    local: SocketAddr,
}

impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addr = first_addr(addr)?;
        let endpoint = to_net_endpoint(addr)?;
        let handle = open_socket(ask_io::net::PROTO_TCP, endpoint.family)?;
        let mut request = [0u8; 23];
        let payload = ask_io::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        // `netstack` dispatches `NET_OP_BIND` and `NET_OP_LISTEN` through the
        // same handler (`serve_tcp_listen`), which binds and starts
        // listening in one step — a single `OP_LISTEN` call is both.
        let listen_completion = socket_call(ask_io::net::OP_LISTEN, payload)?;
        if listen_completion.result < 0 {
            close_handle(handle);
            return Err(io::const_error!(io::ErrorKind::AddrInUse, "netstack: listen failed"));
        }
        Ok(TcpListener { handle, local: addr })
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let mut request = [0u8; 4];
        let payload = ask_io::net::encode_net_handle(&mut request, self.handle);
        let completion = socket_call(ask_io::net::OP_ACCEPT, payload)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        let (new_handle, endpoint) =
            ask_io::net::decode_net_endpoint_request(completion.payload()).ok_or_else(unsupported_err)?;
        let peer = from_net_endpoint(endpoint)?;
        Ok((
            TcpStream {
                handle: new_handle,
                peer,
                read_timeout: Mutex::new(None),
                write_timeout: Mutex::new(None),
            },
            peer,
        ))
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        unsupported()
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_only_v6(&self, _only_v6: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_bool_option(
            self.handle,
            ask_io::net::SocketOption::Nonblocking,
            nonblocking,
        )
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        close_handle(self.handle);
    }
}

#[derive(Debug)]
pub struct UdpSocket {
    handle: u32,
    local: SocketAddr,
    connected: Mutex<Option<SocketAddr>>,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addr = first_addr(addr)?;
        let endpoint = to_net_endpoint(addr)?;
        let handle = open_socket(ask_io::net::PROTO_UDP, endpoint.family)?;
        let mut request = [0u8; 23];
        let payload = ask_io::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        let completion = socket_call(ask_io::net::OP_BIND, payload)?;
        if completion.result < 0 {
            close_handle(handle);
            return Err(io::const_error!(io::ErrorKind::AddrInUse, "netstack: bind failed"));
        }
        Ok(UdpSocket {
            handle,
            local: addr,
            connected: Mutex::new(None),
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        })
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.connected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ok_or_else(|| io::const_error!(io::ErrorKind::NotConnected, "socket is not connected"))
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let timeout = *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let want = (buf.len() as u32).min(ask_io::net::DATA_LEN);
        let buffer = ask_io::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_io::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_RECV_FROM, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        // `OP_RECV_FROM`'s completion payload is the raw 19-byte sender
        // `NetEndpoint` (`netstack`'s `try_udp_recv_from` reply), not a
        // handle-prefixed endpoint request — the byte count travels in
        // `completion.result` instead, matching every other I/O op here.
        let from = ask_io::net::decode_net_endpoint(completion.payload())
            .ok_or_else(unsupported_err)?;
        let from = from_net_endpoint(from)?;
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
            .ok_or_else(unsupported_err)?;
        buf[..n].copy_from_slice(data);
        Ok((n, from))
    }

    pub fn peek_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        let timeout = *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let n = (buf.len() as u32).min(ask_io::net::DATA_LEN) as usize;
        let endpoint = to_net_endpoint(*addr)?;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_io::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 35];
        let payload =
            ask_io::net::encode_net_datagram_request(&mut request, self.handle, buffer, 0, endpoint);
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_SEND_TO, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        Ok(completion.result as usize)
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        unsupported()
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
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

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_bool_option(
            self.handle,
            ask_io::net::SocketOption::Nonblocking,
            nonblocking,
        )
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let want = (buf.len() as u32).min(ask_io::net::DATA_LEN);
        let buffer = ask_io::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_io::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_RECV, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
            .ok_or_else(unsupported_err)?;
        buf[..n].copy_from_slice(data);
        Ok(n)
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let timeout = *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let n = (buf.len() as u32).min(ask_io::net::DATA_LEN) as usize;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_io::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_io::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_io::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let completion = call_with_timeout(&mut guard, ask_io::net::OP_SEND, payload, timeout)?;
        if completion.result < 0 {
            return Err(netstack_error(completion.result));
        }
        Ok(completion.result as usize)
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = first_addr(addr)?;
        let endpoint = to_net_endpoint(addr)?;
        let mut request = [0u8; 23];
        let payload = ask_io::net::encode_net_endpoint_request(&mut request, self.handle, endpoint);
        let completion = socket_call(ask_io::net::OP_CONNECT, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::ConnectionRefused, "netstack: connect failed"));
        }
        *self.connected.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr);
        Ok(())
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        close_handle(self.handle);
    }
}

/// Name resolution goes to the `resolver` service over its own
/// `APP_RESOLVER_TOKEN` lease (`ask_io::resolver`), separate from the
/// `netstack` channel above so a process can be granted sockets without name
/// resolution. Literals never reach the service: they are parsed here, so
/// `connect("127.0.0.1:port")` and `connect("[::1]:port")` work in a process
/// with no resolver lease at all.
pub struct LookupHost {
    addresses: [SocketAddr; ask_io::resolver::MAX_ADDRESSES],
    count: usize,
    index: usize,
}

impl LookupHost {
    fn one(addr: SocketAddr) -> LookupHost {
        let mut addresses = [addr; ask_io::resolver::MAX_ADDRESSES];
        addresses[0] = addr;
        LookupHost { addresses, count: 1, index: 0 }
    }

    fn port(&self) -> u16 {
        self.addresses.first().map_or(0, |addr| addr.port())
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

/// The process-wide channel to `resolver`, created lazily on first name
/// lookup. A process that only ever connects to literals never creates it.
static RESOLVER_CHANNEL: OnceLock<Mutex<SyncChannel>> = OnceLock::new();

fn resolver_channel() -> io::Result<MutexGuard<'static, SyncChannel>> {
    ask_sys::log("resolver PAL: acquiring channel");
    let cell = RESOLVER_CHANNEL.get_or_try_init(|| {
        ask_sys::log("resolver PAL: creating leased channel");
        match SyncChannel::create_leased(
            ask_abi::APP_RESOLVER_TOKEN,
            ask_io::resolver::CHANNEL_PAGES,
        ) {
            Ok(channel) => Ok(Mutex::new(channel)),
            Err(error) => {
                eprintln!(
                    "resolver PAL: lease creation failed token={} pages={} error={error:?}",
                    ask_abi::APP_RESOLVER_TOKEN,
                    ask_io::resolver::CHANNEL_PAGES,
                );
                Err(io::const_error!(
                    io::ErrorKind::NotConnected,
                    "no resolver lease"
                ))
            }
        }
    })?;
    ask_sys::log("resolver PAL: channel ready");
    Ok(cell.lock().unwrap_or_else(|e| e.into_inner()))
}

fn resolver_error(result: i32) -> io::Error {
    match result {
        ask_io::resolver::RESULT_NOT_FOUND => {
            io::const_error!(io::ErrorKind::NotFound, "name not found")
        }
        ask_io::resolver::RESULT_TIMED_OUT => {
            io::const_error!(io::ErrorKind::TimedOut, "name lookup timed out")
        }
        ask_io::resolver::RESULT_DENIED => {
            io::const_error!(io::ErrorKind::PermissionDenied, "name lookup denied")
        }
        ask_io::resolver::RESULT_UNAVAILABLE => {
            io::const_error!(io::ErrorKind::NotConnected, "resolver unavailable")
        }
        _ => io::const_error!(io::ErrorKind::InvalidInput, "resolver rejected the name"),
    }
}

fn resolve_name(host: &str, port: u16) -> io::Result<LookupHost> {
    let mut request = [0u8; ask_io::resolver::REQUEST_MAX_LEN];
    let payload = ask_io::resolver::encode_lookup_request(
        &mut request,
        host,
        port,
        ask_io::resolver::FAMILY_ANY,
    )
    .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "invalid host name"))?;

    let completion = {
        let mut guard = resolver_channel()?;
        guard.call(ask_io::resolver::OP_LOOKUP, payload)?
    };
    if completion.result < 0 {
        return Err(resolver_error(completion.result));
    }
    let reply = ask_io::resolver::LookupReply::decode(completion.payload(), port)
        .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidData, "malformed resolver reply"))?;

    let mut addresses = [SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
        ask_io::resolver::MAX_ADDRESSES];
    let mut count = 0;
    for endpoint in reply {
        let Some(slot) = addresses.get_mut(count) else {
            break;
        };
        *slot = lookup_socket_addr(endpoint)?;
        count += 1;
    }
    if count == 0 {
        return Err(io::Error::NO_ADDRESSES);
    }
    Ok(LookupHost { addresses, count, index: 0 })
}

impl TryFrom<&str> for LookupHost {
    type Error = io::Error;

    fn try_from(host_port: &str) -> io::Result<LookupHost> {
        if let Ok(addr) = host_port.parse::<SocketAddr>() {
            return Ok(LookupHost::one(addr));
        }
        // Split off the port the same way the other platforms' PALs do: the
        // last colon, so an unbracketed IPv6 literal cannot be mistaken for
        // a host:port pair.
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
        if let Some(endpoint) = ask_io::net::endpoint_from_host_port(host, port) {
            return Ok(LookupHost::one(lookup_socket_addr(endpoint)?));
        }
        resolve_name(host, port)
    }
}

pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    LookupHost::try_from((host, port))
}
