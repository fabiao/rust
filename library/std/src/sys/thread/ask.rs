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
use crate::sys::map_ask_error;
use crate::thread::ThreadInit;
use crate::time::{Duration, Instant};

pub const DEFAULT_MIN_STACK_SIZE: usize = 256 * 1024;

/// Bump base for `std::thread`'s own `SpawnThread` stacks — distinct from
/// askposix's pthread stacks (`STACK_BASE` at `0x3a00_…`,
/// `recipes/essentials/askposix/source/src/pthread.rs`) and askposix's
/// dlmalloc heap (`0x3b00_…`), so the two thread implementations sharing a
/// process (a Class 2 binary linking both `std` and C code through
/// `askposix`) can never collide.
const STACK_BASE: u64 = 0x0000_3900_0000_0000;
const PAGE_SIZE: u64 = 4096;

static NEXT_STACK: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(STACK_BASE);

fn align_pages(len: u64) -> u64 {
    len.div_ceil(PAGE_SIZE) * PAGE_SIZE
}

pub struct Thread {
    tid: u64,
    stack_base: u64,
    stack_len: u64,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let stack_len = align_pages(stack as u64);
        let stack_base = NEXT_STACK.fetch_add(stack_len, core::sync::atomic::Ordering::Relaxed);
        ask_abi::map(stack_base, stack_len, true, false).map_err(map_ask_error)?;
        let stack_top = stack_base + stack_len - 8;

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
            ask_abi::exit(0);
        }

        let entry = (entry_shim as *const ()).expose_provenance() as u64;
        match ask_abi::spawn_thread(entry, stack_top) {
            Ok(tid) => Ok(Thread {
                tid: tid as u64,
                stack_base,
                stack_len,
            }),
            Err(e) => {
                let _ = ask_abi::revoke(stack_base, stack_len);
                Err(map_ask_error(e))
            }
        }
    }

    pub fn join(self) {
        // `JoinThread` already blocks until exit and reclaims the tid slot;
        // std's own `Packet`/`join()` contract only needs the wait, not the
        // exit code (that travels back through the `rust_start` closure's
        // own `Packet`, same as every other target's `Thread::join`). The
        // joined thread's own execution context is gone by the time this
        // returns, so reclaiming its stack mapping here (rather than from
        // the thread itself, which cannot unmap its own live stack) is safe.
        let _ = ask_abi::join_thread(self.tid);
        let _ = ask_abi::revoke(self.stack_base, self.stack_len);
    }
}

pub fn yield_now() {
    ask_abi::yield_now();
}

pub fn sleep(duration: Duration) {
    let Some(deadline) = Instant::now().checked_add(duration) else {
        // An unrepresentable deadline is effectively forever. Park in the
        // largest supported chunks, still permitting explicit wakes.
        loop {
            ask_abi::park_timeout(u64::MAX);
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
        ask_abi::park_timeout(millis.max(1));
    }
}
