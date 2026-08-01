//! `std::net` PAL for ask: blocking `TcpStream`/`TcpListener`/`UdpSocket`
//! bridging onto `netstack`'s `NET_OP_*` wire protocol (`ask_abi::net`) over
//! one process-wide `SyncChannel` — `netstack` accepts exactly one client
//! channel for its entire process lifetime (docs/rust-toolchain.md), so
//! every socket handle in this process multiplexes over the same connection
//! rather than opening one channel per socket, unlike `sys/fs/ask.rs`.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use crate::sync::{Mutex, MutexGuard, OnceLock};
use crate::sys::channel::SyncChannel;
use crate::sys::map_ask_error;
use crate::sys::pal::unsupported_err;
use crate::sys::unsupported;
use crate::time::Duration;

/// The process-wide channel to `netstack`, created lazily on first socket
/// use and shared by every `TcpStream`/`TcpListener`/`UdpSocket` handle in
/// this process — see the module doc comment.
static NET_CHANNEL: OnceLock<Mutex<SyncChannel>> = OnceLock::new();

/// Decode the `net:` provider's pid out of the raw startup-view blob — the
/// same trimmed, `libask`-free approach `sys/fs/ask.rs` uses for its `/out`
/// binding, applied to `ask_abi::view`'s separate net-provider block.
fn net_provider_pid() -> io::Result<u32> {
    let mut bytes = [0u8; ask_abi::view::LEN];
    ask_abi::get_startup_view(&mut bytes).map_err(map_ask_error)?;
    let offset = ask_abi::view::NET_PROVIDER_OFFSET;
    let present = *bytes.get(offset + 4).ok_or_else(unsupported_err)?;
    if present == 0 {
        return Err(unsupported_err());
    }
    let pid = bytes
        .get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(unsupported_err)?;
    Ok(pid)
}

fn channel() -> io::Result<MutexGuard<'static, SyncChannel>> {
    let cell = NET_CHANNEL.get_or_try_init(|| {
        let provider_pid = net_provider_pid()?;
        SyncChannel::create(provider_pid as u64, ask_abi::net::CHANNEL_PAGES)
            .map(Mutex::new)
            .map_err(|_| io::const_error!(io::ErrorKind::NotConnected, "netstack unreachable"))
    })?;
    Ok(cell.lock().unwrap_or_else(|e| e.into_inner()))
}

fn to_net_endpoint(addr: SocketAddr) -> io::Result<ask_abi::net::NetEndpoint> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut address = [0u8; 16];
            address[..4].copy_from_slice(&v4.ip().octets());
            Ok(ask_abi::net::NetEndpoint {
                family: ask_abi::net::AF_IPV4,
                port: v4.port(),
                address,
            })
        }
        SocketAddr::V6(_) => Err(unsupported_err()),
    }
}

fn from_net_endpoint(endpoint: ask_abi::net::NetEndpoint) -> io::Result<SocketAddr> {
    if endpoint.family != ask_abi::net::AF_IPV4 {
        return Err(unsupported_err());
    }
    let octets: [u8; 4] = endpoint
        .address
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(unsupported_err)?;
    Ok(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(octets),
        endpoint.port,
    )))
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

fn open_socket(protocol: u8) -> io::Result<u32> {
    let mut request = [0u8; 2];
    let payload = ask_abi::net::encode_net_socket_request(&mut request, ask_abi::net::AF_IPV4, protocol);
    let completion = socket_call(ask_abi::net::OP_SOCKET, payload)?;
    if completion.result < 0 {
        return Err(io::const_error!(io::ErrorKind::Other, "netstack: socket failed"));
    }
    ask_abi::net::decode_net_handle(completion.payload()).ok_or_else(unsupported_err)
}

fn close_handle(handle: u32) {
    let mut request = [0u8; 4];
    let payload = ask_abi::net::encode_net_handle(&mut request, handle);
    let _ = socket_call(ask_abi::net::OP_CLOSE, payload);
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
        let handle = open_socket(ask_abi::net::PROTO_TCP)?;
        let endpoint = to_net_endpoint(*addr)?;
        let mut request = [0u8; 23];
        let payload = ask_abi::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        let timeout = if timeout == Duration::MAX { None } else { Some(timeout) };
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_CONNECT, payload, timeout)?;
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
        let want = (buf.len() as u32).min(ask_abi::net::DATA_LEN);
        let buffer = ask_abi::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_abi::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_RECV, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: recv failed"));
        }
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
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
        let n = (buf.len() as u32).min(ask_abi::net::DATA_LEN) as usize;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_abi::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_abi::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_SEND, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: send failed"));
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
            Shutdown::Read => ask_abi::net::SHUTDOWN_READ,
            Shutdown::Write => ask_abi::net::SHUTDOWN_WRITE,
            Shutdown::Both => ask_abi::net::SHUTDOWN_BOTH,
        };
        let mut request = [0u8; 8];
        let payload =
            ask_abi::net::encode_net_handle_value(&mut request, self.handle, direction as u32);
        let completion = socket_call(ask_abi::net::OP_SHUTDOWN, payload)?;
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

    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        Ok(())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(true)
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

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        unsupported()
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
        let handle = open_socket(ask_abi::net::PROTO_TCP)?;
        let endpoint = to_net_endpoint(addr)?;
        let mut request = [0u8; 23];
        let payload = ask_abi::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        // `netstack` dispatches `NET_OP_BIND` and `NET_OP_LISTEN` through the
        // same handler (`serve_tcp_listen`), which binds and starts
        // listening in one step — a single `OP_LISTEN` call is both.
        let listen_completion = socket_call(ask_abi::net::OP_LISTEN, payload)?;
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
        let payload = ask_abi::net::encode_net_handle(&mut request, self.handle);
        let completion = socket_call(ask_abi::net::OP_ACCEPT, payload)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: accept failed"));
        }
        let (new_handle, endpoint) =
            ask_abi::net::decode_net_endpoint_request(completion.payload()).ok_or_else(unsupported_err)?;
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

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        unsupported()
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
        let handle = open_socket(ask_abi::net::PROTO_UDP)?;
        let endpoint = to_net_endpoint(addr)?;
        let mut request = [0u8; 23];
        let payload = ask_abi::net::encode_net_endpoint_request(&mut request, handle, endpoint);
        let completion = socket_call(ask_abi::net::OP_BIND, payload)?;
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
        let want = (buf.len() as u32).min(ask_abi::net::DATA_LEN);
        let buffer = ask_abi::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_abi::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_RECV_FROM, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: recv_from failed"));
        }
        // `OP_RECV_FROM`'s completion payload is the raw 19-byte sender
        // `NetEndpoint` (`netstack`'s `try_udp_recv_from` reply), not a
        // handle-prefixed endpoint request — the byte count travels in
        // `completion.result` instead, matching every other I/O op here.
        let from = ask_abi::net::decode_net_endpoint(completion.payload())
            .ok_or_else(unsupported_err)?;
        let from = from_net_endpoint(from)?;
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
            .ok_or_else(unsupported_err)?;
        buf[..n].copy_from_slice(data);
        Ok((n, from))
    }

    pub fn peek_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        let timeout = *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let n = (buf.len() as u32).min(ask_abi::net::DATA_LEN) as usize;
        let endpoint = to_net_endpoint(*addr)?;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_abi::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 35];
        let payload =
            ask_abi::net::encode_net_datagram_request(&mut request, self.handle, buffer, 0, endpoint);
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_SEND_TO, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: send_to failed"));
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

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout = *self.read_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let want = (buf.len() as u32).min(ask_abi::net::DATA_LEN);
        let buffer = ask_abi::net::NetBuffer::new(0, want).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_abi::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let mut guard = channel()?;
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_RECV, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: recv failed"));
        }
        let n = (completion.result as usize).min(buf.len());
        let data = guard
            .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
            .ok_or_else(unsupported_err)?;
        buf[..n].copy_from_slice(data);
        Ok(n)
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let timeout = *self.write_timeout.lock().unwrap_or_else(|e| e.into_inner());
        let n = (buf.len() as u32).min(ask_abi::net::DATA_LEN) as usize;
        let mut guard = channel()?;
        {
            let data = guard
                .shared_region_mut(ask_abi::net::DATA_OFFSET as usize, n)
                .ok_or_else(unsupported_err)?;
            data.copy_from_slice(&buf[..n]);
        }
        let buffer = ask_abi::net::NetBuffer::new(0, n as u32).ok_or_else(unsupported_err)?;
        let mut request = [0u8; 16];
        let payload = ask_abi::net::encode_net_io_request(&mut request, self.handle, buffer, 0);
        let completion = call_with_timeout(&mut guard, ask_abi::net::OP_SEND, payload, timeout)?;
        if completion.result < 0 {
            return Err(io::const_error!(io::ErrorKind::Other, "netstack: send failed"));
        }
        Ok(completion.result as usize)
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addr = first_addr(addr)?;
        let endpoint = to_net_endpoint(addr)?;
        let mut request = [0u8; 23];
        let payload = ask_abi::net::encode_net_endpoint_request(&mut request, self.handle, endpoint);
        let completion = socket_call(ask_abi::net::OP_CONNECT, payload)?;
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

/// `netstack` speaks no DNS opcode — only literal-address `ToSocketAddrs`
/// resolution is supported (no name-resolver service exists yet).
pub struct LookupHost {
    addr: Option<SocketAddr>,
}

impl Iterator for LookupHost {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        self.addr.take()
    }
}

impl TryFrom<&str> for LookupHost {
    type Error = io::Error;

    fn try_from(host: &str) -> io::Result<LookupHost> {
        LookupHost::try_from((host, 0))
    }
}

impl<'a> TryFrom<(&'a str, u16)> for LookupHost {
    type Error = io::Error;

    fn try_from((host, port): (&'a str, u16)) -> io::Result<LookupHost> {
        let ip: Ipv4Addr = host
            .parse()
            .map_err(|_| io::const_error!(io::ErrorKind::InvalidInput, "no DNS resolver on ask"))?;
        Ok(LookupHost {
            addr: Some(SocketAddr::V4(SocketAddrV4::new(ip, port))),
        })
    }
}

pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    LookupHost::try_from((host, port))
}
