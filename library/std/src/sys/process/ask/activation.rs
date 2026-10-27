use super::{launcher, map_process_result};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::{io, sys::channel::SyncChannel, time::Duration};
use ask_io::process::{self as wire, result};

static NONCE: AtomicU64 = AtomicU64::new(1);

fn protocol() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}
fn empty() -> ask_fd::BufferSpan {
    ask_fd::BufferSpan { data: core::ptr::null_mut(), length: 0 }
}
fn owner_result(code: u32, error: ask_fd::ErrorRecord) -> io::Result<bool> {
    match code {
        ask_fd::status::COMPLETE if error == ask_fd::ErrorRecord::clear() => Ok(true),
        ask_fd::status::PENDING | ask_fd::status::CONTENDED
            if error == ask_fd::ErrorRecord::clear() =>
        {
            Ok(false)
        }
        _ => Err(crate::sys::fd::error(code, error)),
    }
}

struct Activation<'a> {
    channel: &'a mut SyncChannel,
    nonce: u64,
    preparation: Option<ask_fd::PreparationId>,
    deadline: u64,
    committed: bool,
}
impl Activation<'_> {
    fn remaining(&self) -> io::Result<Duration> {
        let now = ask_sys::get_monotonic_ms();
        let remaining = self
            .deadline
            .checked_sub(now)
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        Ok(Duration::from_millis(remaining))
    }
    fn pause(&self) {
        let _ = ask_sys::park_timeout(crate::sys::channel::POLL_INTERVAL_MS);
    }
    fn exchange(
        &mut self,
        opcode: u32,
        bytes: &[u8],
    ) -> io::Result<crate::sys::channel::Completion> {
        // Accepted activation may commit before its reply; retain the matching completion.
        self.channel.call(opcode, bytes)
    }
    fn command(&mut self, opcode: u32) -> io::Result<i32> {
        let mut bytes = [0; wire::NONCE_REQUEST_LEN];
        let reply = self.exchange(
            opcode,
            wire::encode_nonce_request(&mut bytes, self.nonce).ok_or_else(protocol)?,
        )?;
        if !result::valid(reply.result) || !reply.payload().is_empty() {
            return Err(protocol());
        }
        Ok(reply.result)
    }
    fn query(&mut self, ready: impl Fn(&wire::QueryReply) -> bool) -> io::Result<wire::QueryReply> {
        loop {
            let mut bytes = [0; wire::NONCE_REQUEST_LEN];
            let reply = self.exchange(
                wire::OP_QUERY,
                wire::encode_nonce_request(&mut bytes, self.nonce).ok_or_else(protocol)?,
            )?;
            let query = wire::decode_query_reply(reply.payload()).ok_or_else(protocol)?;
            if query.result != reply.result {
                return Err(protocol());
            }
            if query.result < 0 {
                return Err(map_process_result(query.result));
            }
            if query.nonce != self.nonce {
                return Err(protocol());
            }
            if query.state == wire::state::ABORTED {
                return Err(map_process_result(result::ABORTED));
            }
            if ready(&query) {
                return Ok(query);
            }
            self.pause();
        }
    }
    fn view(&self, phase: u32) -> io::Result<ask_fd::ChildView> {
        let id = self.preparation.ok_or_else(protocol)?;
        loop {
            self.remaining()?;
            let mut view = ask_fd::ChildView {
                phase: 0,
                reserved: 0,
                record: empty(),
                local_resources: empty(),
            };
            let mut error = ask_fd::ErrorRecord::clear();
            let status = unsafe { ask_fd::__askfd_probe_child(&id, &mut view, &mut error) };
            if owner_result(status, error)? {
                if view.reserved != 0 || !ask_fd::child_phase::known(view.phase) {
                    return Err(protocol());
                }
                if view.phase == phase {
                    return Ok(view);
                }
            }
            self.pause();
        }
    }
    fn resources(&mut self, view: ask_fd::ChildView, header: ask_io::fd::Header) -> io::Result<()> {
        let length = view.local_resources.checked_len().ok_or_else(protocol)?;
        let stride = core::mem::size_of::<ask_fd::LocalResource>();
        if length % stride != 0
            || length / stride != header.local_resource_count as usize
            || (length != 0
                && view.local_resources.data.addr()
                    % core::mem::align_of::<ask_fd::LocalResource>()
                    != 0)
        {
            return Err(protocol());
        }
        if length == 0 {
            return Ok(());
        }
        // The preparation retains these immutable source tokens until commit or abort.
        let resources = unsafe {
            core::slice::from_raw_parts(
                view.local_resources.data.cast::<ask_fd::LocalResource>(),
                length / stride,
            )
        };
        let mut cursor = 1;
        for batch in resources.chunks(ask_abi::MAX_BLOCK_TOKENS) {
            let mut pairs = [0; ask_abi::MAX_BLOCK_TOKENS * ask_io::fd::layout::RESOURCE_DEPOSIT];
            let length = batch.len() * ask_io::fd::layout::RESOURCE_DEPOSIT;
            for (resource, bytes) in
                batch.iter().zip(pairs.chunks_mut(ask_io::fd::layout::RESOURCE_DEPOSIT))
            {
                let receipt = ask_sys::delegate_capability_via_channel(
                    self.channel.virt(),
                    resource.token.0,
                    ask_abi::CAP_DELEGATE_COPY,
                    ask_abi::TokenDest::any(),
                    ask_abi::shared_memory_rights::ALL,
                )
                .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
                ask_io::fd::ResourceDeposit { description_key: resource.description_key, receipt }
                    .encode(bytes)
                    .ok_or_else(protocol)?;
            }
            let prefix = ask_io::fd::ResourcePrefix {
                launch_nonce: self.nonce,
                batch_cursor: cursor,
                count: batch.len() as u32,
                complete: u32::from(batch.as_ptr_range().end == resources.as_ptr_range().end),
            };
            loop {
                self.remaining()?;
                self.channel
                    .shared_region_mut(wire::DATA_OFFSET as usize, length)
                    .ok_or_else(protocol)?
                    .copy_from_slice(&pairs[..length]);
                let mut bytes = [0; ask_io::fd::layout::RESOURCE_PREFIX];
                let reply = self
                    .exchange(wire::OP_INTAKE, prefix.encode(&mut bytes).ok_or_else(protocol)?)?;
                let ack = wire::decode_cursor_reply(reply.payload()).ok_or_else(protocol)?;
                if ack.result != reply.result {
                    return Err(protocol());
                }
                if ack.result < 0 {
                    return Err(map_process_result(ack.result));
                }
                if ack.result == result::OK && ack.cursor == cursor {
                    break;
                }
                if ack.result != result::PENDING {
                    return Err(protocol());
                }
                self.pause();
            }
            cursor = cursor.checked_add(1).ok_or_else(protocol)?;
        }
        Ok(())
    }
    fn activate(
        &mut self,
        reserved: wire::ReserveReply,
        actions: &[ask_io::fd::OrderedAction],
    ) -> io::Result<wire::LaunchReply> {
        if reserved.entry_kind == ask_io::entry::kind::HOOKLESS {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        let mut preparation = ask_fd::PreparationId::invalid();
        let mut error = ask_fd::ErrorRecord::clear();
        let span = ask_fd::BufferSpan {
            data: actions.as_ptr().cast_mut().cast(),
            length: actions.len().checked_mul(ask_io::fd::layout::ACTION).ok_or_else(protocol)?
                as u64,
        };
        loop {
            self.remaining()?;
            let status = unsafe {
                ask_fd::__askfd_prepare_child(
                    &reserved.context,
                    &span,
                    actions.len() as u64,
                    &mut preparation,
                    &mut error,
                )
            };
            owner_result(status, error)?;
            if preparation.valid() {
                self.preparation = Some(preparation);
                break;
            }
            if status != ask_fd::status::CONTENDED {
                return Err(protocol());
            }
            self.pause();
        }
        let view = self.view(ask_fd::child_phase::PREPARED)?;
        let length = view.record.checked_len().ok_or_else(protocol)?;
        if length < ask_io::fd::layout::HEADER {
            return Err(protocol());
        }
        // The preparation pins this immutable record until the complete copy is deposited.
        let record = unsafe { core::slice::from_raw_parts(view.record.data.cast::<u8>(), length) };
        let header = ask_io::fd::Header::decode(
            record.get(..ask_io::fd::layout::HEADER).ok_or_else(protocol)?,
        )
        .ok_or_else(protocol)?;
        if header.launch_nonce != self.nonce || header.expected_len() != Some(length) {
            return Err(protocol());
        }
        let mapping = ask_ipc::shm::OwnedMapping::allocate(
            length as u64,
            ask_abi::capability::APP_FRAME_TOKEN,
        )
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        unsafe {
            core::ptr::copy_nonoverlapping(record.as_ptr(), core::ptr::with_exposed_provenance_mut::<u8>(mapping.virt() as usize), length)
        };
        let receipt = mapping
            .deposit(self.channel.virt(), ask_ipc::shm::Access::ReadOnly)
            .map_err(|_| protocol())?;
        let mut bytes = [0; wire::LAUNCH_REQUEST_LEN];
        let reply = self.exchange(
            wire::OP_LAUNCH,
            wire::encode_launch_request(
                &mut bytes,
                wire::LaunchRequest { nonce: self.nonce, record_receipt: receipt },
            )
            .ok_or_else(protocol)?,
        )?;
        let launch = wire::decode_launch_reply(reply.payload()).ok_or_else(protocol)?;
        if launch.result != reply.result {
            return Err(protocol());
        }
        if launch.result < 0 {
            return Err(map_process_result(launch.result));
        }
        self.resources(view, header)?;
        let query = self.query(|value| value.pid != 0)?;
        let birth = wire::ProcessIdentity { pid: query.pid, generation: query.generation };
        loop {
            self.remaining()?;
            let status = unsafe { ask_fd::__askfd_bind_child(&preparation, &birth, &mut error) };
            owner_result(status, error)?;
            if status != ask_fd::status::CONTENDED {
                break;
            }
            self.pause();
        }
        self.view(ask_fd::child_phase::BOUND)?;
        let allowed = self.command(wire::OP_ALLOW)?;
        if allowed != result::OK && allowed != result::PENDING {
            return Err(map_process_result(allowed));
        }
        self.query(|value| value.activation_committed)?;
        self.committed = true;
        self.finish();
        let _ = self.command(wire::OP_ACKNOWLEDGE);
        Ok(wire::LaunchReply {
            result: result::OK,
            pid: birth.pid,
            generation: birth.generation,
        })
    }

    fn finish(&mut self) {
        if let Some(id) = self.preparation {
            loop {
                let mut error = ask_fd::ErrorRecord::clear();
                let status = unsafe { ask_fd::__askfd_finish_child(&id, &mut error) };
                match status {
                    ask_fd::status::COMPLETE if error == ask_fd::ErrorRecord::clear() => { self.preparation = None; return; }
                    ask_fd::status::PENDING | ask_fd::status::CONTENDED => { let _ = ask_sys::park_timeout(crate::sys::channel::POLL_INTERVAL_MS); }
                    _ => crate::sys::abort_internal(),
                }
            }
        }
    }
}
impl Drop for Activation<'_> {
    fn drop(&mut self) {
        if self.committed { self.finish(); return; }
        if !self.committed {
            let mut bytes = [0; wire::NONCE_REQUEST_LEN];
            if let Some(bytes) = wire::encode_nonce_request(&mut bytes, self.nonce) {
                let _ = self.channel.submit(wire::OP_ABORT, bytes);
            }
        }
        if let Some(id) = self.preparation.take() {
            let mut error = ask_fd::ErrorRecord::clear();
            loop {
                let status = unsafe { ask_fd::__askfd_abort_child(&id, &mut error) };
                if status != ask_fd::status::CONTENDED {
                    break;
                }
                let _ = ask_sys::park_timeout(crate::sys::channel::POLL_INTERVAL_MS);
            }
        }
    }
}

pub(super) fn launch(
    name: &[u8],
    argv: &[u8],
    env: &[u8],
    actions: &[ask_io::fd::OrderedAction],
) -> io::Result<wire::LaunchReply> {
    let nonce = NONCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let total = name
        .len()
        .checked_add(argv.len())
        .and_then(|length| length.checked_add(env.len()))
        .filter(|length| *length <= wire::DATA_LEN as usize)
        .ok_or_else(protocol)?;
    let mut channel = launcher()?;
    let window =
        channel.shared_region_mut(wire::DATA_OFFSET as usize, total).ok_or_else(protocol)?;
    let env_offset = name.len() + argv.len();
    window[..name.len()].copy_from_slice(name);
    window[name.len()..env_offset].copy_from_slice(argv);
    window[env_offset..].copy_from_slice(env);
    let request = wire::ReserveRequest {
        nonce,
        flags: wire::launch_flag::FOREGROUND | wire::launch_flag::REQUIRE_DESCRIPTORS
            | wire::launch_flag::PRESERVE_ARGV0,
        name: wire::Buffer::new(0, name.len() as u32).ok_or_else(protocol)?,
        argv: wire::Buffer::new(name.len() as u32, argv.len() as u32).ok_or_else(protocol)?,
        env: wire::Buffer::new(env_offset as u32, env.len() as u32).ok_or_else(protocol)?,
    };
    let mut transaction = Activation {
        channel: &mut channel,
        nonce,
        preparation: None,
        deadline: u64::MAX,
        committed: false,
    };
    let mut bytes = [0; wire::RESERVE_REQUEST_LEN];
    let reply = transaction.exchange(
        wire::OP_RESERVE,
        wire::encode_reserve_request(&mut bytes, request).ok_or_else(protocol)?,
    )?;
    let reserved = wire::decode_reserve_reply(reply.payload()).ok_or_else(protocol)?;
    if reserved.result != reply.result {
        return Err(protocol());
    }
    if reserved.result != result::OK {
        return Err(map_process_result(reserved.result));
    }
    if reserved.context.launch_nonce != nonce {
        return Err(protocol());
    }
    transaction.deadline = reserved.context.adoption_deadline_ms;
    transaction.activate(reserved, actions)
}
