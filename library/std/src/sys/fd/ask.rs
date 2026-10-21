use ask_fd::{AliasTicket, BufferSpan, ErrorRecord, IoRequest, IoResult, OperationId, status};

use crate::future::Future;
use crate::io;
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use crate::task::{Context, Poll, Waker};

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
        let result = blocking(perform(
            self.as_raw_fd(),
            request(ask_fd::action::READ, output.len()),
            &[],
            output,
        ))??;
        usize::try_from(result.completed_count)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }
    pub fn write(&self, input: &[u8]) -> io::Result<usize> {
        let result = blocking(perform(
            self.as_raw_fd(),
            request(ask_fd::action::WRITE, input.len()),
            input,
            &mut [],
        ))??;
        usize::try_from(result.completed_count)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }
    pub fn flush(&self) -> io::Result<()> {
        blocking(perform(self.as_raw_fd(), request(ask_fd::action::FLUSH, 0), &[], &mut []))?
            .map(|_| ())
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

pub(crate) fn retry(mut operation: impl FnMut(&mut ErrorRecord) -> u32) -> io::Result<()> {
    loop {
        let mut record = ErrorRecord::clear();
        match operation(&mut record) {
            status::COMPLETE => return Ok(()),
            status::CONTENDED
                if crate::rt::ask_entry::execution_class() == ask_fd::execution_class::CLASS2 =>
            {
                let _ = ask_sys::park_timeout(crate::sys::channel::POLL_INTERVAL_MS);
            }
            code => return Err(error(code, record)),
        }
    }
}

pub(crate) fn close(number: i32) {
    if retry(|error| unsafe { ask_fd::__askfd_close(number, error) }).is_err() {
        crate::sys::abort_internal();
    }
}

pub(crate) fn duplicate(number: i32) -> io::Result<OwnedFd> {
    let mut output = -1;
    let mut ticket = AliasTicket::invalid();
    retry(|error| unsafe {
        ask_fd::__askfd_duplicate(
            number,
            3,
            ask_fd::descriptor_flags::CLOSE_ON_EXEC,
            &mut output,
            &mut ticket,
            error,
        )
    })?;
    let mut raw = -1;
    if retry(|error| unsafe { ask_fd::__askfd_relinquish_alias(&ticket, &mut raw, error) }).is_err()
    {
        crate::sys::abort_internal();
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

pub(crate) fn bind(binding: ask_fd::BindingRecord) -> io::Result<FileDesc> {
    let mut output = -1;
    let mut ticket = AliasTicket::invalid();
    retry(|error| unsafe {
        ask_fd::__askfd_bind(
            &binding,
            3,
            ask_fd::descriptor_flags::CLOSE_ON_EXEC,
            &mut output,
            &mut ticket,
            error,
        )
    })?;
    let mut raw = -1;
    if retry(|error| unsafe { ask_fd::__askfd_relinquish_alias(&ticket, &mut raw, error) }).is_err()
    {
        crate::sys::abort_internal();
    }
    Ok(unsafe { FileDesc::from_raw_fd(raw) })
}

pub(crate) struct MountSource {
    id: ask_fd::SourceId,
    pub record: ask_fd::SourceRecord,
}

impl Drop for MountSource {
    fn drop(&mut self) {
        if !self.id.valid() { return; }
        let mut error = ErrorRecord::clear();
        if unsafe { ask_fd::__askfd_release_source(&self.id, &mut error) } != status::COMPLETE {
            crate::sys::abort_internal();
        }
    }
}

fn lookup_mount_source(token: u32) -> io::Result<MountSource> {
    let mount = token
        .checked_sub(ask_abi::APP_MOUNT_TOKEN_BASE)
        .filter(|mount| *mount < ask_abi::view::BINDINGS as u32)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut id = ask_fd::SourceId::invalid();
    let mut record = ask_fd::SourceRecord {
        token: ask_abi::CapabilityToken(0),
        backend: 0,
        reserved: 0,
        padding: 0,
        provider_pid: 0,
        provider_generation: 0,
    };
    retry(|error| unsafe { ask_fd::__askfd_lookup_mount(mount, &mut id, &mut record, error) })?;
    Ok(MountSource { id, record })
}

fn reconnect_error(error: ask_ipc::filesystem::Error) -> io::Error {
    use ask_ipc::filesystem::Error;
    use ask_io::process::result;
    let kind = match error {
        Error::Expired => io::ErrorKind::TimedOut,
        Error::Protocol => io::ErrorKind::InvalidData,
        Error::Rejected(result::DENIED) => io::ErrorKind::PermissionDenied,
        Error::Rejected(result::UNSUPPORTED) => io::ErrorKind::Unsupported,
        Error::Rejected(result::CAPACITY) => io::ErrorKind::OutOfMemory,
        Error::Rejected(result::BUSY) => io::ErrorKind::WouldBlock,
        _ => io::ErrorKind::BrokenPipe,
    };
    io::Error::from(kind)
}

pub(crate) fn mount_source(token: u32) -> io::Result<MountSource> {
    let previous = lookup_mount_source(token)?;
    if ask_sys::get_process_generation(previous.record.provider_pid).ok()
        == Some(previous.record.provider_generation) { return Ok(previous); }
    let slot = token.checked_sub(ask_abi::APP_MOUNT_TOKEN_BASE)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mount = if slot as usize == ask_io::view::MountPoint::In.slot() {
        ask_io::process::mount::IN
    } else if slot as usize == ask_io::view::MountPoint::Apps.slot() {
        ask_io::process::mount::APPS
    } else { return Err(io::Error::from(io::ErrorKind::BrokenPipe)); };
    let deadline = ask_sys::get_monotonic_ms().checked_add(ask_ipc::directory::policy::DEADLINE_MS)
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
    let mut exchange = ask_ipc::filesystem::Reconnect::new(mount, deadline).map_err(reconnect_error)?;
    let endpoint = loop {
        if let Some(endpoint) = exchange.probe(ask_sys::get_monotonic_ms()).map_err(reconnect_error)? { break endpoint; }
        let remaining = deadline.saturating_sub(ask_sys::get_monotonic_ms());
        if remaining == 0 { return Err(io::Error::from(io::ErrorKind::TimedOut)); }
        let _ = ask_sys::park_timeout(remaining.min(crate::sys::channel::POLL_INTERVAL_MS));
    };
    let record = ask_fd::SourceRecord {
        token: ask_abi::CapabilityToken(endpoint.authority.token().ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?),
        backend: ask_io::fd::backend::FILE, reserved: 0, padding: 0,
        provider_pid: endpoint.provider.pid, provider_generation: endpoint.provider.generation,
    };
    let mut replacement = MountSource { id: ask_fd::SourceId::invalid(), record };
    retry(|error| unsafe { ask_fd::__askfd_adopt_source(&record, &mut replacement.id, error) })?;
    let _ = endpoint.authority.into_token();
    let mut error_record = ErrorRecord::clear();
    let code = unsafe { ask_fd::__askfd_bind_mount(slot, &previous.id, &replacement.id, &mut error_record) };
    if code == status::COMPLETE { replacement.id = ask_fd::SourceId::invalid(); }
    else if code != status::CONFLICT { return Err(error(code, error_record)); }
    lookup_mount_source(token)
}

pub(crate) fn bind_native(
    source: MountSource,
    channel: ask_ipc::SyncChannel,
    pages: u64,
    reply: ask_io::fd::RedeemReply,
) -> io::Result<FileDesc> {
    let record = source.record;
    let binding = ask_fd::BindingRecord {
        byte_length: ask_fd::layout::BINDING_RECORD as u64,
        backend: record.backend,
        rights: reply.granted_rights,
        provider_pid: record.provider_pid,
        provider_generation: record.provider_generation,
        attachment: reply.attachment,
        source: source.id,
        mapping_base: channel.virt(),
        mapping_pages: pages,
        local_object_generation: 0,
        reserved: 0,
    };
    let output = bind(binding)?;
    core::mem::forget(source);
    core::mem::forget(channel);
    Ok(output)
}

pub(crate) fn null() -> io::Result<FileDesc> {
    use core::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
    let mapping = ask_ipc::shm::OwnedMapping::allocate(
        ask_abi::address_space::PAGE_BYTES,
        ask_abi::APP_FRAME_TOKEN,
    )
    .map_err(crate::sys::map_ask_error)?;
    let state = ask_io::fd::LocalState {
        object_generation: generation,
        backend: ask_io::fd::backend::NULL,
        reserved: 0,
        status: 0,
        reserved_tail: 0,
    };
    unsafe {
        core::ptr::with_exposed_provenance_mut::<ask_io::fd::LocalState>(mapping.virt() as usize)
            .write(state);
    }
    let binding = ask_fd::BindingRecord {
        byte_length: ask_fd::layout::BINDING_RECORD as u64,
        backend: ask_io::fd::backend::NULL,
        rights: ask_io::fd::rights::READ | ask_io::fd::rights::WRITE | ask_io::fd::rights::TRANSFER,
        provider_pid: 0,
        provider_generation: 0,
        attachment: ask_io::fd::AttachmentRef { id: 0, generation: 0, object_generation: 0 },
        source: ask_fd::SourceId::invalid(),
        mapping_base: mapping.virt(),
        mapping_pages: mapping.len() / ask_abi::address_space::PAGE_BYTES,
        local_object_generation: generation,
        reserved: 0,
    };
    let output = bind(binding)?;
    core::mem::forget(mapping);
    Ok(output)
}

pub(crate) fn call(
    channel: &mut ask_ipc::SyncChannel,
    opcode: u32,
    bytes: &[u8],
    deadline: u64,
) -> io::Result<ask_ipc::Completion> {
    let remaining = deadline
        .checked_sub(ask_sys::get_monotonic_ms())
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
    channel.submit(opcode, bytes, None).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    channel
        .recv_completion_parked(Some(remaining), ask_ipc::sync::WaitHooks::RAW)
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?
        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
}

pub(crate) fn redeem(
    provider: ask_abi::ProcessIdentity,
    grant: ask_io::fd::GrantRef,
    backend: u32,
    rights: u32,
    deadline: u64,
) -> io::Result<FileDesc> {
    use ask_io::directory::introduction;
    let mut capman = ask_ipc::SyncChannel::create_leased(ask_abi::CAPMAN_CONTROL_TOKEN, 1)
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    let mut bytes = [0; introduction::CONNECT_LEN];
    let payload = introduction::IntroductionRef { target: provider, key: grant.id }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let reply = call(&mut capman, introduction::CONNECT, payload, deadline)?;
    let reply = introduction::ConnectReply::decode(reply.payload())
        .filter(|reply| reply.status == introduction::status::OK && reply.token != 0)
        .ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?;
    let source = ask_sys::capability::OwnedToken::from_token(reply.token);
    let mut channel = ask_ipc::SyncChannel::create_leased(reply.token, 1)
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    let mut bytes = [0; ask_io::fd::layout::REDEEM];
    let payload = ask_io::fd::Redeem { grant, requested_rights: rights, reserved: 0 }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let opcode = ask_io::fd::attachment::opcodes(backend)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
        .redeem;
    let reply = loop {
        let completion = call(&mut channel, opcode, payload, deadline)?;
        if completion.result != 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let (status, body) = ask_io::fd::lifecycle_status::decode_reply(
            completion.payload(),
            ask_io::fd::layout::REDEEM_REPLY,
        )
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        if status == ask_io::fd::lifecycle_status::PENDING {
            let remaining = deadline
                .checked_sub(ask_sys::get_monotonic_ms())
                .filter(|remaining| *remaining != 0)
                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
            let _ = ask_sys::park_timeout(remaining.min(crate::sys::channel::POLL_INTERVAL_MS));
            continue;
        }
        if status != ask_io::fd::lifecycle_status::OK {
            return Err(lifecycle_error(status));
        }
        break ask_io::fd::RedeemReply::decode(body)
            .filter(|reply| {
                reply.granted_rights == rights
                    && reply.attachment.object_generation == grant.object_generation
            })
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    };
    let mut source_id = ask_fd::SourceId::invalid();
    let record = ask_fd::SourceRecord {
        token: ask_abi::CapabilityToken(reply_token(&source)?),
        backend,
        reserved: 0,
        padding: 0,
        provider_pid: provider.pid,
        provider_generation: provider.generation,
    };
    retry(|error| unsafe { ask_fd::__askfd_adopt_source(&record, &mut source_id, error) })?;
    let _ = source.into_token();
    let binding = ask_fd::BindingRecord {
        byte_length: ask_fd::layout::BINDING_RECORD as u64,
        backend,
        rights,
        provider_pid: provider.pid,
        provider_generation: provider.generation,
        attachment: reply.attachment,
        source: source_id,
        mapping_base: channel.virt(),
        mapping_pages: 1,
        local_object_generation: 0,
        reserved: 0,
    };
    match bind(binding) {
        Ok(output) => {
            core::mem::forget(channel);
            Ok(output)
        }
        Err(error) => {
            let mut record = ErrorRecord::clear();
            if unsafe { ask_fd::__askfd_release_source(&source_id, &mut record) }
                != status::COMPLETE
            {
                crate::sys::abort_internal();
            }
            Err(error)
        }
    }
}

fn reply_token(source: &ask_sys::capability::OwnedToken) -> io::Result<u32> {
    source.token().ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))
}

pub(crate) fn error(code: u32, record: ErrorRecord) -> io::Error {
    if code == status::NATIVE_FAILURE && record.domain == ask_fd::error_domain::FILESYSTEM {
        return crate::sys::fs::fs_error(
            record.native_code as i32,
            "native filesystem operation failed",
        );
    }
    if code == status::NATIVE_FAILURE && record.domain == ask_fd::error_domain::STREAM {
        use ask_io::pipe::buffered::result;
        return io::Error::from(match record.native_code as i32 {
            result::WOULD_BLOCK => io::ErrorKind::WouldBlock,
            result::NO_READER | result::LOST => io::ErrorKind::BrokenPipe,
            result::CAPACITY => io::ErrorKind::QuotaExceeded,
            result::INVALID => io::ErrorKind::InvalidInput,
            result::UNSUPPORTED => io::ErrorKind::Unsupported,
            result::CANCELLED => io::ErrorKind::Interrupted,
            _ => io::ErrorKind::Other,
        });
    }
    let kind = match code {
        status::PENDING | status::CONTENDED => io::ErrorKind::WouldBlock,
        status::INVALID | status::STALE | status::RANGE => io::ErrorKind::InvalidInput,
        status::DENIED => io::ErrorKind::PermissionDenied,
        status::UNSUPPORTED => io::ErrorKind::Unsupported,
        status::BACKING => io::ErrorKind::OutOfMemory,
        status::TRANSPORT_LOST => io::ErrorKind::BrokenPipe,
        status::CANCELLED => io::ErrorKind::Interrupted,
        status::QUOTA => io::ErrorKind::QuotaExceeded,
        _ => io::ErrorKind::Other,
    };
    io::Error::from(kind)
}

pub(crate) fn lifecycle_error(code: u32) -> io::Error {
    use ask_io::fd::lifecycle_status as native;
    io::Error::from(match code {
        native::DENIED => io::ErrorKind::PermissionDenied,
        native::LOST => io::ErrorKind::BrokenPipe,
        native::CONFLICT | native::MALFORMED => io::ErrorKind::InvalidInput,
        native::QUOTA | native::GENERATION_EXHAUSTED => io::ErrorKind::QuotaExceeded,
        native::BACKING => io::ErrorKind::OutOfMemory,
        native::UNSUPPORTED => io::ErrorKind::Unsupported,
        native::CANCELLED => io::ErrorKind::Interrupted,
        _ => io::ErrorKind::Other,
    })
}

struct CapturedAlias {
    ticket: AliasTicket,
}

impl CapturedAlias {
    fn acquire(number: i32) -> io::Result<Self> {
        let mut ticket = AliasTicket::invalid();
        retry(|error| unsafe { ask_fd::__askfd_acquire_alias(number, &mut ticket, error) })?;
        Ok(Self { ticket })
    }
}

impl Drop for CapturedAlias {
    fn drop(&mut self) {
        let mut number = -1;
        let mut record = ErrorRecord::clear();
        let code = unsafe {
            ask_fd::__askfd_relinquish_alias(&self.ticket, &mut number, &mut record)
        };
        if code == status::COMPLETE {
            return;
        }
        // Concurrent close leaves the borrowed old generation pinned. Release
        // that detached ticket without looking up its possibly reused number.
        if code == status::INVALID
            && record.domain == ask_fd::error_domain::OWNER
            && record.native_code == ask_fd::owner_error::BAD_FD
            && unsafe { ask_fd::__askfd_release_alias(&self.ticket, &mut record) }
                == status::COMPLETE
        {
            return;
        }
        crate::sys::abort_internal();
    }
}

struct Operation {
    id: OperationId,
    _alias: CapturedAlias,
}

impl Drop for Operation {
    fn drop(&mut self) {
        let mut output = ErrorRecord::clear();
        if unsafe { ask_fd::__askfd_abandon_io(&self.id, &mut output) } != status::COMPLETE {
            crate::sys::abort_internal();
        }
    }
}

pub(crate) async fn perform(
    number: i32,
    request: IoRequest,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<IoResult> {
    let alias = CapturedAlias::acquire(number)?;
    let mut record = ErrorRecord::clear();
    let span = BufferSpan { data: input.as_ptr().cast_mut().cast(), length: input.len() as u64 };
    let mut id = OperationId::invalid();
    let code = unsafe {
        ask_fd::__askfd_begin_io(
            &alias.ticket,
            &request,
            &span,
            u64::from(!input.is_empty()),
            &mut id,
            &mut record,
        )
    };
    if code != status::COMPLETE && code != status::PENDING {
        return Err(error(code, record));
    }
    let operation = Operation { id, _alias: alias };
    let span = BufferSpan { data: output.as_mut_ptr().cast(), length: output.len() as u64 };
    crate::future::poll_fn(|cx| {
        let mut value = IoResult {
            status: status::PENDING,
            reserved: 0,
            completed_count: 0,
            resulting_position: 0,
            error: ErrorRecord::clear(),
        };
        let code =
            unsafe { ask_fd::__askfd_probe_io(&operation.id, &span, &mut value, &mut record) };
        match code {
            status::PENDING => Poll::Pending,
            status::CONTENDED => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            status::COMPLETE if value.status == status::COMPLETE => Poll::Ready(Ok(value)),
            status::COMPLETE => Poll::Ready(Err(error(value.status, value.error))),
            _ => Poll::Ready(Err(error(code, record))),
        }
    })
    .await
}

pub(crate) fn request(action: u32, count: usize) -> IoRequest {
    IoRequest {
        action,
        position: if action == ask_fd::action::READ || action == ask_fd::action::WRITE {
            ask_fd::position::DESCRIPTION_CURSOR
        } else {
            ask_fd::position::NONE
        },
        absolute_offset: 0,
        seek_delta: 0,
        requested_count: count as u64,
        native_opcode: 0,
        reserved: 0,
    }
}

pub(crate) async fn native(
    number: i32,
    opcode: u32,
    payload: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let request = IoRequest {
        action: ask_fd::action::NATIVE_CONTROL,
        position: ask_fd::position::NONE,
        absolute_offset: 0,
        seek_delta: 0,
        requested_count: output.len() as u64,
        native_opcode: opcode,
        reserved: 0,
    };
    let result = perform(number, request, payload, output).await?;
    usize::try_from(result.completed_count).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

pub(crate) async fn metadata(number: i32, output: &mut [u8]) -> io::Result<ask_fd::MetadataResult> {
    let alias = CapturedAlias::acquire(number)?;
    let mut id = OperationId::invalid();
    let mut record = ErrorRecord::clear();
    let code = unsafe { ask_fd::__askfd_metadata(&alias.ticket, &mut id, &mut record) };
    if code != status::PENDING && code != status::COMPLETE {
        return Err(error(code, record));
    }
    let operation = Operation { id, _alias: alias };
    let span = BufferSpan { data: output.as_mut_ptr().cast(), length: output.len() as u64 };
    crate::future::poll_fn(|cx| {
        let mut result = ask_fd::MetadataResult {
            status: status::PENDING,
            encoding: 0,
            byte_length: 0,
            error: ErrorRecord::clear(),
        };
        let code = unsafe {
            ask_fd::__askfd_probe_metadata(&operation.id, &span, &mut result, &mut record)
        };
        match code {
            status::PENDING => Poll::Pending,
            status::CONTENDED => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            status::COMPLETE if result.status == status::COMPLETE => Poll::Ready(Ok(result)),
            status::COMPLETE => Poll::Ready(Err(error(result.status, result.error))),
            code => Poll::Ready(Err(error(code, record))),
        }
    })
    .await
}

pub(crate) fn is_terminal(number: i32) -> bool {
    let mut bytes = [0; ask_io::fd::layout::OBJECT_METADATA];
    match blocking(metadata(number, &mut bytes)) {
        Ok(Ok(result)) if result.encoding == ask_fd::metadata_encoding::OBJECT => {
            ask_io::fd::ObjectMetadata::decode(&bytes)
                .is_some_and(|metadata| metadata.kind == ask_io::fd::backend::TERMINAL)
        }
        _ => false,
    }
}

pub(crate) fn blocking<F: Future>(future: F) -> io::Result<F::Output> {
    if crate::rt::ask_entry::execution_class() == ask_fd::execution_class::CLASS1 {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    let mut future = core::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
        let _ = ask_sys::park_timeout(crate::sys::channel::POLL_INTERVAL_MS);
    }
}
