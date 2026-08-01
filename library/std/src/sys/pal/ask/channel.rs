//! `SyncChannel`: a blocking, executor-free client over the same SQ/CQ wire
//! format `libask::channel::Channel` uses (`ask_abi::channel`) — std has no
//! `libask` dependency and no async executor, so this reimplements the
//! submit/complete/pop logic directly against raw `ask_abi::syscall` calls,
//! spin-parking on the caller's own completion instead of awaiting a
//! `Future`. Referenced as `sys::pal::ask::channel::SyncChannel` by
//! `sys/fs/ask.rs` and `sys/net/connection/ask.rs`.

use ask_abi::channel::{
    CQ_CAPACITY, CQ_ENTRIES_OFFSET, CQ_HEADER_OFFSET, CQ_PAYLOAD_OFFSET, Cqe, LAYOUT_LEN,
    MAX_MSG_LEN, Ring, RingHeader, SQ_CAPACITY, SQ_ENTRIES_OFFSET, SQ_HEADER_OFFSET,
    SQ_PAYLOAD_OFFSET, SharedBufferHeader, Sqe,
};

use crate::io;
use crate::sys::pal::unsupported_err;
use crate::time::Duration;

/// Re-check cadence for a blocked completion wait — same order of magnitude
/// as `netstack`'s own idle-loop `IDLE_PARK_MS` (25ms), chosen for the same
/// reason: short enough that a missed wake (`Park`'s documented lost-wake
/// caveat) or a "nothing else runnable, stay `Running`" scheduler edge case
/// resolves within a human-imperceptible bound rather than wedging, long
/// enough to keep the busy-poll overhead negligible against real IPC
/// round-trip latency.
const POLL_INTERVAL_MS: u64 = 25;

/// One complete `[header][payload]` message popped off a ring, copied out of
/// shared memory so it carries no borrow of `SyncChannel`.
pub struct Completion {
    pub user_data: u64,
    pub result: i32,
    buf: [u8; MAX_MSG_LEN],
    len: usize,
}

impl Completion {
    pub fn payload(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }
}

/// A bidirectional shared-memory channel with a synchronous, spin-park wait
/// for completions — the std-PAL counterpart to `libask::channel::Channel`,
/// built directly on `ask_abi::syscall` since std cannot depend on `libask`'s
/// `Future`/executor machinery.
pub struct SyncChannel {
    base: *mut u8,
    pages: u64,
    peer_pid: u64,
    sq: Ring<Sqe, SQ_CAPACITY>,
    cq: Ring<Cqe, CQ_CAPACITY>,
    next_sq_seq: u64,
    expect_cq_seq: u64,
}

/// `SyncChannel` is used from exactly one thread at a time by construction
/// (one `File` owns its own channel; `net`'s process-wide channel is guarded
/// by a `Mutex` — see `sys/net/connection/ask.rs`), but the raw pointers
/// inside prevent the compiler from inferring `Send` on their own.
unsafe impl Send for SyncChannel {}

impl SyncChannel {
    /// Establish a channel with `peer_pid` and initialize the ring layout —
    /// the requester side, matching `libask::channel::Channel::create`.
    pub fn create(peer_pid: u64, pages: u64) -> io::Result<Self> {
        let virt = ask_abi::channel_create(peer_pid, pages).map_err(super::map_ask_error)?;
        let mut ch = Self::attach(virt, peer_pid, pages);
        ch.init();
        Ok(ch)
    }

    fn attach(virt: u64, peer_pid: u64, pages: u64) -> Self {
        let base = core::ptr::with_exposed_provenance_mut::<u8>(virt as usize);
        // Safety: offsets are fixed compile-time constants within this
        // channel's own mapped region; both sides compute the identical
        // layout (`ask_abi::channel`'s shared constants), and `LAYOUT_LEN`
        // fits the pages the caller requested.
        let sq_header = unsafe { base.add(SQ_HEADER_OFFSET) as *mut RingHeader };
        let sq_entries = unsafe { base.add(SQ_ENTRIES_OFFSET) as *mut Sqe };
        let cq_header = unsafe { base.add(CQ_HEADER_OFFSET) as *mut RingHeader };
        let cq_entries = unsafe { base.add(CQ_ENTRIES_OFFSET) as *mut Cqe };
        Self {
            base,
            pages,
            peer_pid,
            // Safety: pointers computed above from this channel's own mapped
            // region; both peers agree on `SQ_CAPACITY`/`CQ_CAPACITY`.
            sq: unsafe { Ring::new(sq_header, sq_entries) },
            cq: unsafe { Ring::new(cq_header, cq_entries) },
            next_sq_seq: 0,
            expect_cq_seq: 0,
        }
    }

    /// Only the creator initializes the ring headers, before the peer's
    /// `ChannelAccept` can observe the memory — mirrors
    /// `libask::channel::Channel::init`.
    fn init(&mut self) {
        // Safety: called only from `create`, before the peer attaches —
        // exclusive access to the whole region at this point.
        unsafe {
            (self.base.add(SQ_HEADER_OFFSET) as *mut RingHeader).write(RingHeader::new());
            (self.base.add(CQ_HEADER_OFFSET) as *mut RingHeader).write(RingHeader::new());
        }
    }

    /// A mutable view into a protocol-defined portion of the shared mapping
    /// (e.g. `net`'s page-1 data window), bounds-checked against the pages
    /// actually mapped.
    pub fn shared_region_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        let mapped_len = self.pages.checked_mul(4096)? as usize;
        let end = offset.checked_add(len)?;
        if end > mapped_len {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts_mut(self.base.add(offset), len) })
    }

    fn slot_bytes(&self, absolute_offset: usize) -> &mut [u8] {
        // Safety: `absolute_offset` is always one of `SQ_PAYLOAD_OFFSET +
        // idx * MAX_MSG_LEN` or `CQ_PAYLOAD_OFFSET + idx * MAX_MSG_LEN` for
        // `idx < {SQ,CQ}_CAPACITY`, which stays within `LAYOUT_LEN`.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(absolute_offset), MAX_MSG_LEN) }
    }

    /// Push a message onto this channel's SQ and wake the peer, returning
    /// the `user_data` correlating the eventual completion.
    pub fn submit(&mut self, opcode: u32, payload: &[u8]) -> io::Result<u64> {
        let msg_len = ask_abi::channel::HEADER_LEN + payload.len();
        if msg_len > MAX_MSG_LEN {
            return Err(unsupported_err());
        }
        let idx = self
            .sq
            .producer_slot_index_if_free()
            .ok_or_else(unsupported_err)?;
        let offset = SQ_PAYLOAD_OFFSET + idx * MAX_MSG_LEN;

        let seq = self.next_sq_seq;
        self.next_sq_seq += 1;
        SharedBufferHeader::write(self.slot_bytes(offset), seq, payload)
            .ok_or_else(unsupported_err)?;

        self.sq
            .try_push(Sqe::new(seq, opcode, offset as u32, msg_len as u32))
            .map_err(|_| unsupported_err())?;
        ask_abi::wake(self.peer_pid).map_err(super::map_ask_error)?;
        Ok(seq)
    }

    /// Bounds-check a popped entry's `msg_offset`/`msg_len` against the
    /// payload window it must lie in.
    fn checked_msg_bounds(
        msg_offset: u32,
        msg_len: u32,
        window_start: usize,
        slots: usize,
    ) -> Option<(usize, usize)> {
        let offset = msg_offset as usize;
        let len = msg_len as usize;
        let end = offset.checked_add(len)?;
        if len > MAX_MSG_LEN || offset < window_start || end > window_start + slots * MAX_MSG_LEN {
            return None;
        }
        Some((offset, len))
    }

    /// Pop and validate the next pending completion, if any — never blocks.
    pub fn try_pop_completion(&mut self) -> io::Result<Option<Completion>> {
        let Some(cqe) = self.cq.try_pop() else {
            return Ok(None);
        };
        let (offset, len) =
            Self::checked_msg_bounds(cqe.msg_offset, cqe.msg_len, CQ_PAYLOAD_OFFSET, CQ_CAPACITY)
                .ok_or_else(unsupported_err)?;
        // Safety: bounds proven above to lie within this channel's own
        // mapped payload window.
        let raw = unsafe { core::slice::from_raw_parts(self.base.add(offset), len) };
        let payload = SharedBufferHeader::validate(raw, self.expect_cq_seq)
            .map_err(|_| unsupported_err())?;
        self.expect_cq_seq += 1;
        let mut buf = [0u8; MAX_MSG_LEN];
        let len = payload.len();
        buf.get_mut(..len)
            .ok_or_else(unsupported_err)?
            .copy_from_slice(payload);
        Ok(Some(Completion {
            user_data: cqe.user_data,
            result: cqe.result,
            buf,
            len,
        }))
    }

    /// Block until the completion matching `user_data` arrives. Polls on a
    /// short `ParkTimeout` cadence (`POLL_INTERVAL_MS`) rather than an
    /// unbounded `Park`: `Park`'s wake-lost-if-not-yet-Waiting race
    /// (`recipes/core/kernel/source/src/proc/mod.rs`'s own documented
    /// caveat) is normally closed by checking pending work and parking
    /// under the same lock — a mechanism only the kernel side can provide,
    /// not this userspace client — and `park_locked` additionally stays
    /// `Running` outright (never truly yielding) if nothing else is
    /// `Ready` at that exact instant, which starved this process's own
    /// peer server (e.g. `askfs`, itself `Waiting` on `devman`, never got
    /// re-scheduled) during real boot testing. A bounded re-check trades a
    /// small constant polling cost for guaranteed forward progress. The
    /// server side answers requests in submission order and this channel
    /// carries at most one in-flight request at a time (every FS/NET client
    /// here submits, then waits, before submitting again), so the first
    /// completion popped is always the right one — but the `user_data`
    /// check stays as a defensive assertion against a future
    /// multi-in-flight caller.
    pub fn wait_for_completion(&mut self, user_data: u64) -> io::Result<Completion> {
        loop {
            if let Some(completion) = self.try_pop_completion()? {
                if completion.user_data == user_data {
                    return Ok(completion);
                }
                // Not our reply (shouldn't happen under single-in-flight
                // use) — drop it and keep waiting rather than losing the
                // caller's own answer forever.
                continue;
            }
            ask_abi::park_timeout(POLL_INTERVAL_MS);
        }
    }

    /// Like `wait_for_completion`, but bounded by a real wall-clock deadline
    /// — `None` waits forever. Returns `Err(ErrorKind::TimedOut)` if the
    /// deadline elapses with no matching completion. Uses the same bounded
    /// `POLL_INTERVAL_MS` cadence as `wait_for_completion` for each park, so
    /// a single long-remaining `park_timeout` call can't itself wedge on the
    /// lost-wake/nothing-runnable interaction described there.
    pub fn wait_for_completion_timeout(
        &mut self,
        user_data: u64,
        timeout: Option<Duration>,
    ) -> io::Result<Completion> {
        let Some(timeout) = timeout else {
            return self.wait_for_completion(user_data);
        };
        let deadline_ms = ask_abi::get_monotonic_ms().saturating_add(timeout.as_millis() as u64);
        loop {
            if let Some(completion) = self.try_pop_completion()? {
                if completion.user_data == user_data {
                    return Ok(completion);
                }
                continue;
            }
            let remaining = deadline_ms.saturating_sub(ask_abi::get_monotonic_ms());
            if remaining == 0 {
                return Err(io::const_error!(io::ErrorKind::TimedOut, "ask channel wait timed out"));
            }
            ask_abi::park_timeout(remaining.min(POLL_INTERVAL_MS));
        }
    }

    /// `submit` then `wait_for_completion` in one call — the shape every FS/
    /// NET operation in the new PAL uses.
    pub fn call(&mut self, opcode: u32, payload: &[u8]) -> io::Result<Completion> {
        let user_data = self.submit(opcode, payload)?;
        self.wait_for_completion(user_data)
    }

    /// Like `call`, bounded by an optional wall-clock deadline — see
    /// `wait_for_completion_timeout`.
    pub fn call_timeout(
        &mut self,
        opcode: u32,
        payload: &[u8],
        timeout: Option<Duration>,
    ) -> io::Result<Completion> {
        let user_data = self.submit(opcode, payload)?;
        self.wait_for_completion_timeout(user_data, timeout)
    }
}

impl Drop for SyncChannel {
    fn drop(&mut self) {
        // Safety: unmaps only pages this process mapped itself via
        // `channel_create`.
        let _ = ask_abi::revoke(self.base.expose_provenance() as u64, self.pages * 4096);
    }
}

const _: () = assert!(LAYOUT_LEN <= 2 * 4096, "SyncChannel assumes at most 2 mapped pages");
