//! Thread lifecycle for ask, over the kernel's `SpawnThread`/`GetTid`/
//! `JoinThread` primitives (docs/scheduling.md): a `SpawnThread` child shares
//! this process's address space (no new `AddressSpace`/CR3), so `Thread` here
//! is deliberately lean — one caller-mapped stack, one raw entry pointer, no
//! per-thread control block. This mirrors `askposix::pthread`'s own
//! `spawn_thread`/`entry_shim`/naked-trampoline shape (donor: that module's
//! `pthread_create`), minus everything POSIX-only (detach state, TLS key
//! destructors, deferred cancellation) `std::thread` doesn't need — kept
//! separate rather than factored into a shared `askabi` helper, since the
//! two callers' init payloads (`std::thread::ThreadInit` vs askposix's
//! `ThreadControl`) differ in shape and neither side benefits from a shared
//! abstraction thin enough to still avoid coupling `askabi` to either.
//! `available_parallelism`/`current_os_id`/`set_name` stay on
//! `sys::thread::unsupported` (no CPU-topology or per-thread naming syscall
//! exists yet).

use crate::io;
use crate::sync::nonpoison::Mutex;
use crate::sys::map_ask_error;
use crate::thread::ThreadInit;
use crate::time::{Duration, Instant};

pub const DEFAULT_MIN_STACK_SIZE: usize = 256 * 1024;

/// Window for `std::thread`'s own `SpawnThread` stacks — distinct from
/// askposix's pthread stacks (`STACK_BASE` at `0x3a00_…`,
/// `recipes/essentials/askposix/source/src/pthread.rs`) and askposix's
/// dlmalloc heap (`0x3b00_…`), so the two thread implementations sharing a
/// process (a Class 2 binary linking both `std` and C code through
/// `askposix`) can never collide.
const STACK_BASE: u64 = 0x0000_3900_0000_0000;
const STACK_LIMIT: u64 = 0x0000_3a00_0000_0000;
const PAGE_SIZE: u64 = 4096;
/// One reservation per possible ASK execution-context parameter slot.
const STACK_SLOTS: usize = ask_abi::param::WINDOW_SLOTS as usize;
/// Rust-owned stacks reserve one unmapped page below the downward-growing stack.
const STACK_GUARD_LEN: u64 = PAGE_SIZE;

#[derive(Clone, Copy)]
struct StackReservation {
    start: u64,
    end: u64,
    state: StackState,
}

#[derive(Clone, Copy)]
enum StackState {
    Reserved,
    Attached,
    Detached(u64),
    Reaped,
}

struct StackRanges {
    live: [Option<StackReservation>; STACK_SLOTS],
}

impl StackRanges {
    const fn new() -> Self {
        Self { live: [None; STACK_SLOTS] }
    }

    fn allocate(&mut self, stack_len: u64) -> Option<u64> {
        self.reap_detached();
        let reservation_len = stack_len.checked_add(STACK_GUARD_LEN)?;
        if self.live.iter().all(Option::is_some) {
            return None;
        }
        let mut start = STACK_BASE;
        for _ in 0..=STACK_SLOTS {
            let end = start.checked_add(reservation_len)?;
            if end > STACK_LIMIT {
                return None;
            }
            let collision_end = self
                .live
                .iter()
                .flatten()
                .filter(|reservation| start < reservation.end && reservation.start < end)
                .map(|reservation| reservation.end)
                .max();
            match collision_end {
                Some(next) => start = next,
                None => {
                    let slot = self.live.iter().position(Option::is_none)?;
                    self.live[slot] =
                        Some(StackReservation { start, end, state: StackState::Reserved });
                    return start.checked_add(STACK_GUARD_LEN);
                }
            }
        }
        None
    }

    fn activate(&mut self, stack_base: u64, stack_len: u64) -> bool {
        self.update_state(stack_base, stack_len, StackState::Attached)
    }

    fn detach(&mut self, stack_base: u64, stack_len: u64, tid: u64) -> bool {
        self.update_state(stack_base, stack_len, StackState::Detached(tid))
    }

    fn mark_reaped(&mut self, stack_base: u64, stack_len: u64) -> bool {
        self.update_state(stack_base, stack_len, StackState::Reaped)
    }

    fn update_state(&mut self, stack_base: u64, stack_len: u64, state: StackState) -> bool {
        let Some(start) = stack_base.checked_sub(STACK_GUARD_LEN) else {
            return false;
        };
        let Some(end) = stack_base.checked_add(stack_len) else {
            return false;
        };
        let Some(reservation) = self
            .live
            .iter_mut()
            .flatten()
            .find(|reservation| reservation.start == start && reservation.end == end)
        else {
            return false;
        };
        reservation.state = state;
        true
    }

    fn reap_detached(&mut self) {
        for slot in 0..self.live.len() {
            let Some(reservation) = self.live[slot] else {
                continue;
            };
            let reaped = match reservation.state {
                StackState::Detached(tid) => {
                    ask_sys::try_join_thread(tid).is_ok_and(|code| code.is_some())
                }
                StackState::Reaped => true,
                StackState::Reserved | StackState::Attached => false,
            };
            if !reaped {
                continue;
            }
            let Some(live) = self.live[slot].as_mut() else {
                continue;
            };
            live.state = StackState::Reaped;
            let stack_base = reservation.start + STACK_GUARD_LEN;
            let stack_len = reservation.end - stack_base;
            if ask_sys::revoke(stack_base, stack_len).is_ok() {
                self.live[slot] = None;
            }
        }
    }

    fn release(&mut self, stack_base: u64, stack_len: u64) -> bool {
        let Some(start) = stack_base.checked_sub(STACK_GUARD_LEN) else {
            return false;
        };
        let Some(end) = stack_base.checked_add(stack_len) else {
            return false;
        };
        let Some(slot) = self.live.iter().position(|reservation| {
            reservation
                .is_some_and(|reservation| reservation.start == start && reservation.end == end)
        }) else {
            return false;
        };
        self.live[slot] = None;
        true
    }
}

static STACK_RANGES: Mutex<StackRanges> = Mutex::new(StackRanges::new());

fn align_pages(len: u64) -> Option<u64> {
    len.checked_add(PAGE_SIZE - 1).map(|value| value & !(PAGE_SIZE - 1))
}

pub struct Thread {
    tid: u64,
    stack_base: u64,
    stack_len: u64,
    reaped: bool,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let stack_len = align_pages(stack as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "thread stack overflow"))?;
        if stack_len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty thread stack"));
        }
        let stack_base = STACK_RANGES.lock().allocate(stack_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::OutOfMemory, "thread stack window exhausted")
        })?;
        if let Err(error) =
            ask_sys::map(stack_base, stack_len, true, false, ask_abi::APP_FRAME_TOKEN)
        {
            let _ = STACK_RANGES.lock().release(stack_base, stack_len);
            return Err(map_ask_error(error));
        }
        let Some(stack_top) = stack_base.checked_add(stack_len).and_then(|end| end.checked_sub(8))
        else {
            if ask_sys::revoke(stack_base, stack_len).is_ok() {
                let _ = STACK_RANGES.lock().release(stack_base, stack_len);
            }
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "thread stack overflow"));
        };

        // Transfers ownership of `init` into the new thread's own address
        // space view — the trampoline below reconstructs the `Box` and
        // drops it, so this leak is temporary, not permanent.
        let init_ptr = Box::into_raw(init).expose_provenance() as u64;
        // Safety: `stack_top` was just `Map`d writable by this same call;
        // the new thread hasn't started yet, so no concurrent access race.
        unsafe {
            let slot = core::ptr::with_exposed_provenance_mut::<u64>(stack_top as usize);
            core::ptr::write_volatile(slot, init_ptr);
        }

        #[unsafe(naked)]
        extern "sysv64" fn entry_shim() -> ! {
            core::arch::naked_asm!(
                "mov rdi, qword ptr [rsp]",
                "jmp {}",
                sym trampoline,
            );
        }

        extern "sysv64" fn trampoline(init_ptr: u64) -> ! {
            // A `SpawnThread` child always starts with `%fs` base `0`
            // (unset) regardless of the parent's own `%fs` — install this
            // thread's own TLS-key table before anything below touches a
            // `thread_local!` (including `init.init()`'s own `set_current`
            // call), the same reasoning `sys::pal::ask::init` documents for
            // the process's original thread.
            crate::sys::thread_local::key::init_this_thread();
            // Safety: `new` transferred exclusive ownership of this
            // allocation to the new thread via the stack slot above.
            let init = unsafe {
                Box::from_raw(core::ptr::with_exposed_provenance_mut::<ThreadInit>(
                    init_ptr as usize,
                ))
            };
            let rust_start = init.init();
            rust_start();
            // ask has no OS-provided automatic TLS-destructor callback
            // (`sys/thread_local/guard/mod.rs`'s ask arm) — run this
            // thread's own destructors and free its TLS table directly,
            // mirroring `sys/thread/xous.rs`'s identical call.
            unsafe { crate::sys::thread_local::key::destroy_tls() };
            ask_sys::exit(0);
        }

        let entry = (entry_shim as *const ()).expose_provenance() as u64;
        match ask_sys::spawn_thread(entry, stack_top) {
            Ok(tid) => {
                let tid = tid as u64;
                let activated = STACK_RANGES.lock().activate(stack_base, stack_len);
                debug_assert!(activated);
                Ok(Thread { tid, stack_base, stack_len, reaped: false })
            }
            Err(e) => {
                // Safety: spawn failed before publishing the only other owner.
                unsafe {
                    drop(Box::from_raw(core::ptr::with_exposed_provenance_mut::<ThreadInit>(
                        init_ptr as usize,
                    )))
                };
                if ask_sys::revoke(stack_base, stack_len).is_ok() {
                    let _ = STACK_RANGES.lock().release(stack_base, stack_len);
                }
                Err(map_ask_error(e))
            }
        }
    }

    pub fn join(mut self) {
        #[cfg(ask_class1_verify)]
        panic!("Class 1 verification tripwire: std::thread::JoinHandle::join is prohibited");
        // `JoinThread` already blocks until exit and reclaims the tid slot;
        // std's own `Packet`/`join()` contract only needs the wait, not the
        // exit code (that travels back through the `rust_start` closure's
        // own `Packet`, same as every other target's `Thread::join`). The
        // joined thread's own execution context is gone by the time this
        // returns, so reclaiming its stack mapping here (rather than from
        // the thread itself, which cannot unmap its own live stack) is safe.
        ask_sys::join_thread(self.tid).expect("failed to join ASK thread");
        self.reaped = true;
        let _ = STACK_RANGES.lock().mark_reaped(self.stack_base, self.stack_len);
        if ask_sys::revoke(self.stack_base, self.stack_len).is_ok() {
            let _ = STACK_RANGES.lock().release(self.stack_base, self.stack_len);
        }
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = STACK_RANGES.lock().detach(self.stack_base, self.stack_len, self.tid);
        }
    }
}

pub fn yield_now() {
    ask_sys::yield_now();
}

pub fn sleep(duration: Duration) {
    #[cfg(ask_class1_verify)]
    panic!("Class 1 verification tripwire: std::thread::sleep is prohibited");
    let Some(deadline) = Instant::now().checked_add(duration) else {
        // An unrepresentable deadline is effectively forever. Park in the
        // largest supported chunks, still permitting explicit wakes.
        loop {
            ask_sys::park_timeout(u64::MAX);
        }
    };

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        let millis = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(u128::from(u64::MAX)) as u64;
        ask_sys::park_timeout(millis.max(1));
    }
}
