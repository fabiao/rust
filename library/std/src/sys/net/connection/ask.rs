//! Native sockets use the process resident descriptor owner.

use ask_io::net as wire;

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{
    Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs,
};
use crate::os::fd::FromRawFd;
use crate::sync::Mutex;
use crate::sys::fd::{self, FileDesc};
use crate::sys::pal::unsupported_err;
use crate::sys::unsupported;
use crate::time::Duration;

const SOCKET_RIGHTS: u32 = ask_io::fd::rights::ACCESS | ask_io::fd::rights::TRANSFER;

fn invalid() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}
fn first_addr<A: ToSocketAddrs>(addr: A) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?.next().ok_or(io::Error::NO_ADDRESSES)
}
fn to_net_endpoint(addr: SocketAddr) -> io::Result<wire::NetEndpoint> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut address = [0; 16];
            address[..4].copy_from_slice(&v4.ip().octets());
            Ok(wire::NetEndpoint { family: wire::AF_IPV4, port: v4.port(), address })
        }
        SocketAddr::V6(v6) if v6.scope_id() == 0 && v6.flowinfo() == 0 => Ok(wire::NetEndpoint {
            family: wire::AF_IPV6,
            port: v6.port(),
            address: v6.ip().octets(),
        }),
        _ => Err(unsupported_err()),
    }
}
fn lookup_socket_addr(endpoint: wire::NetEndpoint) -> io::Result<SocketAddr> {
    match endpoint.family {
        wire::AF_IPV4 => {
            let bytes: [u8; 4] =
                endpoint.address.get(..4).and_then(|v| v.try_into().ok()).ok_or_else(invalid)?;
            Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(bytes), endpoint.port)))
        }
        wire::AF_IPV6 => Ok(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(endpoint.address),
            endpoint.port,
            0,
            0,
        ))),
        _ => Err(invalid()),
    }
}
fn constructor_error(error: ask_ipc::attachment::Error) -> io::Error {
    use ask_ipc::attachment::Error as E;
    match error {
        E::Owner { status, record } => fd::error(status, record),
        E::Lifecycle(code) => fd::lifecycle_error(code),
        E::Transport(_) => io::Error::from(io::ErrorKind::BrokenPipe),
        E::Expired => io::Error::from(io::ErrorKind::TimedOut),
        E::Introduction(_) => io::Error::from(io::ErrorKind::PermissionDenied),
        E::Protocol => invalid(),
        E::Native(code) => netstack_error(code),
    }
}
fn netstack_error(code: i32) -> io::Error {
    fd::error(
        ask_fd::status::NATIVE_FAILURE,
        ask_fd::ErrorRecord {
            domain: ask_fd::error_domain::NETWORK,
            native_code: code as u32,
            detail: 0,
        },
    )
}

fn deadline() -> u64 {
    ask_sys::get_monotonic_ms().saturating_add(ask_ipc::directory::policy::DEADLINE_MS)
}
fn alias_options() -> ask_ipc::attachment::AliasOptions {
    ask_ipc::attachment::AliasOptions {
        minimum: ask_io::fd::number::AFTER_STANDARD,
        close_on_exec: true,
    }
}
fn bind_opened(opened: ask_ipc::attachment::Opened) -> io::Result<FileDesc> {
    let raw = opened
        .bind(alias_options())
        .map_err(constructor_error)?
        .into_raw()
        .map_err(|(_bound, error)| constructor_error(error))?;
    Ok(unsafe { FileDesc::from_raw_fd(raw) })
}
fn open_socket(protocol: u8, family: u8, end: u64) -> io::Result<FileDesc> {
    fd::blocking(async {
        let bound = ask_ipc::net::socket_blocking(
            protocol,
            family,
            SOCKET_RIGHTS,
            alias_options(),
            end,
            ask_ipc::sync::WaitHooks::RAW,
        )
        .map_err(|error| match error {
            ask_ipc::net::Error::Attachment(error) => constructor_error(error),
            ask_ipc::net::Error::Directory(_) => io::Error::from(io::ErrorKind::PermissionDenied),
        })?;
        let raw = bound.into_raw().map_err(|(_bound, error)| constructor_error(error))?;
        Ok(unsafe { FileDesc::from_raw_fd(raw) })
    })?
}

fn control(
    socket: &FileDesc,
    opcode: u32,
    input: &[u8],
    output: &mut [u8],
    timeout: Option<Duration>,
) -> io::Result<usize> {
    fd::blocking_with_timeout(fd::native(socket.as_raw_fd(), opcode, input, output), timeout)?
}
fn endpoint_control(
    socket: &FileDesc,
    opcode: u32,
    endpoint: wire::NetEndpoint,
    timeout: Option<Duration>,
) -> io::Result<()> {
    let mut bytes = [0; wire::ENDPOINT_LEN];
    let payload = wire::encode_net_endpoint_request(&mut bytes, endpoint).ok_or_else(invalid)?;
    control(socket, opcode, payload, &mut [], timeout).map(|_| ())
}
fn endpoint_option(socket: &FileDesc, option: wire::SocketOption) -> io::Result<SocketAddr> {
    let mut bytes = [0; wire::ENDPOINT_LEN];
    let mut request = [0; 4];
    let payload = wire::encode_net_getopt_request(&mut request, option);
    let count = control(socket, wire::OP_GETOPT, payload, &mut bytes, None)?;
    lookup_socket_addr(
        wire::decode_net_endpoint(bytes.get(..count).ok_or_else(invalid)?).ok_or_else(invalid)?,
    )
}
fn set_bool_option(socket: &FileDesc, option: wire::SocketOption, value: bool) -> io::Result<()> {
    let mut request = [0; wire::OPTION_HEADER_LEN + 1];
    let bytes = [u8::from(value)];
    let payload =
        wire::encode_net_setopt_request(&mut request, option, &bytes).ok_or_else(invalid)?;
    control(socket, wire::OP_SETOPT, payload, &mut [], None).map(|_| ())
}
fn get_bool_option(socket: &FileDesc, option: wire::SocketOption) -> io::Result<bool> {
    let mut request = [0; 4];
    let mut output = [0; 1];
    let count = control(
        socket,
        wire::OP_GETOPT,
        wire::encode_net_getopt_request(&mut request, option),
        &mut output,
        None,
    )?;
    wire::decode_net_bool_option(output.get(..count).ok_or_else(invalid)?).ok_or_else(invalid)
}
fn take_error(socket: &FileDesc) -> io::Result<Option<io::Error>> {
    let mut request = [0; 4];
    let mut output = [0; 4];
    let count = control(
        socket,
        wire::OP_GETOPT,
        wire::encode_net_getopt_request(&mut request, wire::SocketOption::Error),
        &mut output,
        None,
    )?;
    if count != output.len() {
        return Err(invalid());
    }
    let code = i32::from_le_bytes(output);
    Ok((code != wire::result::OK).then(|| netstack_error(code)))
}
fn read(
    socket: &FileDesc,
    output: &mut [u8],
    timeout: Option<Duration>,
    datagram: bool,
) -> io::Result<ask_fd::IoResult> {
    let mut request = fd::request(ask_fd::action::READ, output.len());
    if datagram {
        request.native_opcode = wire::OP_RECV_FROM;
    }
    fd::blocking_with_timeout(fd::perform(socket.as_raw_fd(), request, &[], output), timeout)?
}
fn write(
    socket: &FileDesc,
    input: &[u8],
    timeout: Option<Duration>,
    destination: Option<SocketAddr>,
) -> io::Result<usize> {
    let mut request = fd::request(ask_fd::action::WRITE, input.len());
    if let Some(destination) = destination {
        request.native_opcode = wire::OP_SEND_TO;
        request.endpoint = to_net_endpoint(destination)?.into();
    }
    let result = fd::blocking_with_timeout(
        fd::perform(socket.as_raw_fd(), request, input, &mut []),
        timeout,
    )??;
    usize::try_from(result.completed_count).map_err(|_| invalid())
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
        let endpoint = to_net_endpoint(*addr)?;
        let expiry = (timeout != Duration::MAX).then(|| {
            ask_sys::get_monotonic_ms()
                .saturating_add(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX).max(1))
        });
        let socket = open_socket(
            wire::PROTO_TCP,
            endpoint.family,
            expiry.map_or_else(deadline, |end| end.min(deadline())),
        )?;
        let remaining = expiry
            .map(|end| {
                end.checked_sub(ask_sys::get_monotonic_ms())
                    .filter(|left| *left != 0)
                    .map(Duration::from_millis)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
            })
            .transpose()?;
        endpoint_control(&socket, wire::OP_CONNECT, endpoint, remaining)?;
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
        let result = read(&self.socket, buf, self.read_timeout()?, false)?;
        usize::try_from(result.completed_count).map_err(|_| invalid())
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
        write(&self.socket, buf, self.write_timeout()?, None)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        endpoint_option(&self.socket, wire::SocketOption::PeerEndpoint)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        endpoint_option(&self.socket, wire::SocketOption::LocalEndpoint)
    }

    pub fn shutdown(&self, shutdown: Shutdown) -> io::Result<()> {
        let direction = match shutdown {
            Shutdown::Read => wire::SHUTDOWN_READ,
            Shutdown::Write => wire::SHUTDOWN_WRITE,
            Shutdown::Both => wire::SHUTDOWN_BOTH,
        };
        control(&self.socket, wire::OP_SHUTDOWN, &u32::from(direction).to_le_bytes(), &mut [], None)
            .map(|_| ())
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
        set_bool_option(&self.socket, ask_io::net::SocketOption::NoDelay, nodelay)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        get_bool_option(&self.socket, ask_io::net::SocketOption::NoDelay)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        take_error(&self.socket)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_bool_option(&self.socket, ask_io::net::SocketOption::Nonblocking, nonblocking)
    }
}

#[derive(Debug)]
pub struct TcpListener {
    socket: FileDesc,
}
impl TcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let endpoint = to_net_endpoint(first_addr(addr)?)?;
        let socket = open_socket(wire::PROTO_TCP, endpoint.family, deadline())?;
        endpoint_control(&socket, wire::OP_BIND, endpoint, None)?;
        control(&socket, wire::OP_LISTEN, &[], &mut [], None)?;
        Ok(Self { socket })
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        endpoint_option(&self.socket, wire::SocketOption::LocalEndpoint)
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        fd::blocking(async {
            let mut metadata_bytes = [0; ask_io::fd::layout::OBJECT_METADATA];
            let metadata = fd::metadata(self.socket.as_raw_fd(), &mut metadata_bytes).await?;
            if metadata.encoding != ask_fd::metadata_encoding::OBJECT
                || metadata.byte_length != metadata_bytes.len() as u64
            {
                return Err(invalid());
            }
            let object = ask_io::fd::ObjectMetadata::decode(&metadata_bytes).ok_or_else(invalid)?;
            let required =
                ask_io::fd::metadata_attributes::IDENTITY | ask_io::fd::metadata_attributes::RIGHTS;
            if object.kind != ask_io::fd::backend::SOCKET
                || object.attributes & required != required
                || object.rights & SOCKET_RIGHTS != SOCKET_RIGHTS
            {
                return Err(invalid());
            }
            let nonblocking = get_bool_option(&self.socket, wire::SocketOption::Nonblocking)?;
            let mut ready = if nonblocking {
                None
            } else {
                Some(fd::Readiness::new(self.socket.as_raw_fd(), ask_fd::readiness::READABLE)?)
            };
            loop {
                if let Some(ready) = ready.as_mut() {
                    ready.wait().await?;
                }
                let end = deadline();
                let mut bytes = [0; wire::ACCEPT_REQUEST_LEN];
                let payload = wire::AcceptRequest {
                    deadline: end,
                    rights: SOCKET_RIGHTS,
                    flags: wire::io_flags::NONBLOCKING,
                }
                .encode(&mut bytes)
                .ok_or_else(invalid)?;
                let mut output = [0; wire::ACCEPT_REPLY_LEN];
                let request = fd::request(ask_fd::action::ACCEPT, output.len());
                let (result, creator) = match fd::perform_retained(
                    self.socket.as_raw_fd(),
                    request,
                    payload,
                    &mut output,
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock && !nonblocking => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let count = usize::try_from(result.completed_count).map_err(|_| invalid())?;
                let accepted = wire::AcceptReply::decode(output.get(..count).ok_or_else(invalid)?)
                    .ok_or_else(invalid)?;
                let mut redeem = ask_ipc::attachment::Redeem::new(
                    ask_abi::ProcessIdentity {
                        pid: object.provider_pid,
                        generation: object.provider_generation,
                    },
                    accepted.grant,
                    ask_io::fd::backend::SOCKET,
                    SOCKET_RIGHTS,
                    end,
                )
                .map_err(constructor_error)?;
                let opened =
                    crate::future::poll_fn(|_| match redeem.probe(ask_sys::get_monotonic_ms()) {
                        Ok(Some(opened)) => crate::task::Poll::Ready(Ok(opened)),
                        Ok(None) => crate::task::Poll::Pending,
                        Err(error) => crate::task::Poll::Ready(Err(constructor_error(error))),
                    })
                    .await?;
                let socket = bind_opened(opened)?;
                drop(creator);
                let peer = lookup_socket_addr(accepted.peer.endpoint().ok_or_else(invalid)?)?;
                return Ok((
                    TcpStream {
                        socket,
                        read_timeout: Mutex::new(None),
                        write_timeout: Mutex::new(None),
                    },
                    peer,
                ));
            }
        })?
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
        set_bool_option(&self.socket, ask_io::net::SocketOption::Nonblocking, nonblocking)
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
        let endpoint = to_net_endpoint(first_addr(addr)?)?;
        let socket = open_socket(wire::PROTO_UDP, endpoint.family, deadline())?;
        endpoint_control(&socket, wire::OP_BIND, endpoint, None)?;
        Ok(Self { socket, read_timeout: Mutex::new(None), write_timeout: Mutex::new(None) })
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        endpoint_option(&self.socket, wire::SocketOption::PeerEndpoint)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        endpoint_option(&self.socket, wire::SocketOption::LocalEndpoint)
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let result = read(&self.socket, buf, self.read_timeout()?, true)?;
        let count = usize::try_from(result.completed_count).map_err(|_| invalid())?;
        let peer = lookup_socket_addr(result.endpoint.endpoint().ok_or_else(invalid)?)?;
        Ok((count, peer))
    }

    pub fn peek_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        write(&self.socket, buf, self.write_timeout()?, Some(*addr))
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
        take_error(&self.socket)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_bool_option(&self.socket, ask_io::net::SocketOption::Nonblocking, nonblocking)
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let result = read(&self.socket, buf, self.read_timeout()?, false)?;
        usize::try_from(result.completed_count).map_err(|_| invalid())
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        write(&self.socket, buf, self.write_timeout()?, None)
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        endpoint_control(&self.socket, wire::OP_CONNECT, to_net_endpoint(first_addr(addr)?)?, None)
    }
}

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

fn resolver_channel() -> io::Result<ask_ipc::SyncChannel> {
    let mut endpoint = ask_ipc::directory::connect(
        ask_io::directory::EndpointUse::Once,
        ask_io::resolver::INTERFACE_NAME,
        ask_io::resolver::INTERFACE_VERSION,
        0,
        ask_ipc::sync::WaitHooks::RAW,
    )
    .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
    endpoint
        .create_channel(ask_io::resolver::CHANNEL_PAGES)
        .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))
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

    let completion = fd::blocking_with_timeout(
        async {
            let mut channel = resolver_channel()?;
            let identity = channel
                .submit(ask_io::resolver::OP_LOOKUP, payload, None)
                .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))?;
            crate::future::poll_fn(|_| match channel.try_recv_completion() {
                Ok(Some(completion)) if completion.user_data == identity => {
                    crate::task::Poll::Ready(Ok(completion))
                }
                Ok(Some(_)) => crate::task::Poll::Ready(Err(invalid())),
                Ok(None) => crate::task::Poll::Pending,
                Err(_) => {
                    crate::task::Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected)))
                }
            })
            .await
        },
        Some(Duration::from_millis(ask_ipc::directory::policy::DEADLINE_MS)),
    )??;
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
