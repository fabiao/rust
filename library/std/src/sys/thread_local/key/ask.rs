//! OS-style TLS keys for ask, donor: `sys/thread_local/key/xous.rs`
//! (same shape — a fixed per-thread table of key→value pointers, addressed
//! through one per-thread base pointer). Xous reads a hardware `$tp`
//! register to find its per-thread table; ask has no such register, but
//! `SetFsBase` (`kernel/src/proc/mod.rs`'s `fs_base` field, restored on
//! every context switch) gives every execution context its own private
//! `%fs`-relative storage, so `%fs:0` is used the same way: a self-pointer
//! to this thread's own TLS-key table, mirroring `askposix::pthread`'s
//! identical `ThreadControl::self_ptr`-at-`%fs:0` convention
//! (`recipes/essentials/askposix/source/src/pthread.rs`) — two independent
//! Class 2 runtimes on ask, `std` and askposix, both landed on the same
//! "self-pointer at `%fs:0`" idiom for the same reason (no ELF TLS
//! relocations on this target, `has_thread_local: false`).
//!
//! Neither runtime coordinates with the other: a process linking both real
//! `std::thread_local!` state and `askposix::pthread` on the same thread
//! would have whichever one installs its self-pointer last silently
//! overwrite the other's `%fs` base. No binary in this tree does that today
//! (every current `askposix` consumer is `#![no_std]`, so `std`'s own TLS
//! backend never runs in the same process), but docs/rust-toolchain.md's
//! Class 2 table lists "applications and `askposix` consumers" as one
//! category, so this is flagged rather than silently left to surprise a
//! future mixed binary.

use crate::alloc::{self, Layout, System};
use crate::ptr;
use crate::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use crate::sync::atomic::{Atomic, AtomicPtr, AtomicUsize};

pub type Key = usize;
pub type Dtor = unsafe extern "C" fn(*mut u8);

/// Slot count, including the unused index 0 — same order of magnitude as
/// askposix's own `pthread` key table (`KEY_SLOTS`, 8) plus std's own
/// internal keys (`current()`'s thread handle, the panic count, …); sized
/// generously since one slot is 8 bytes and this table is heap-allocated
/// once per thread, not a fixed page like Xous's.
const MAX_KEYS: usize = 128;

static NEXT_KEY: Atomic<usize> = AtomicUsize::new(1);
static DTORS: Atomic<*mut Node> = AtomicPtr::new(ptr::null_mut());

#[inline]
fn table_layout() -> Layout {
    Layout::array::<*mut u8>(MAX_KEYS).unwrap()
}

/// Reads this thread's TLS-table pointer from `%fs:0` — null until this
/// thread's first `set`/`get` lazily allocates and installs one.
#[inline]
fn table_ptr_addr() -> *mut *mut u8 {
    let mut base: u64;
    unsafe {
        core::arch::asm!("mov {}, fs:[0]", out(reg) base);
    }
    core::ptr::with_exposed_provenance_mut::<*mut u8>(base as usize)
}

#[inline]
fn tls_table() -> &'static mut [*mut u8] {
    let table = table_ptr_addr();
    if !table.is_null() {
        unsafe { core::slice::from_raw_parts_mut(table, MAX_KEYS) }
    } else {
        tls_table_slow()
    }
}

/// Installs this thread's TLS-key table unconditionally — for a thread that
/// has never called `SetFsBase`, so `%fs` base is still the kernel's
/// process-start default (`0`) and reading `fs:[0]` to check for an
/// already-installed table (what `tls_table`'s lazy path does) would
/// page-fault rather than read as a soft null. `sys::pal::ask::init` calls
/// this once for the process's original thread; `sys::thread::ask::Thread`'s
/// own spawn trampoline is a `SpawnThread` child's equivalent call site.
pub(crate) fn init_this_thread() {
    tls_table_slow();
}

#[cold]
fn tls_table_slow() -> &'static mut [*mut u8] {
    // Safety: `System` allocation, zero-initialized, sized for `MAX_KEYS`
    // raw pointers — matches this function's own return type's contract.
    let table = unsafe { alloc::GlobalAlloc::alloc_zeroed(&System, table_layout()) };
    if table.is_null() {
        alloc::handle_alloc_error(table_layout());
    }
    if ask_abi::set_fs_base(table.expose_provenance() as u64).is_err() {
        // Safety: `table` was just allocated with `table_layout()` above and
        // is being abandoned since this thread cannot use it without a
        // working `%fs` base.
        unsafe { alloc::GlobalAlloc::dealloc(&System, table, table_layout()) };
        panic!("ask: SetFsBase failed while installing thread-local storage");
    }
    // Safety: `%fs:0` is this table's own reserved self-pointer slot — the
    // table's first `*mut u8`-sized element, never handed out as a `Key`
    // (`NEXT_KEY` starts at 1).
    unsafe {
        (table as *mut *mut u8).write(table);
    }
    unsafe { core::slice::from_raw_parts_mut(table as *mut *mut u8, MAX_KEYS) }
}

#[inline]
pub fn create(dtor: Option<Dtor>) -> Key {
    let key = NEXT_KEY.fetch_add(1, Relaxed);
    assert!(key < MAX_KEYS, "ask: thread-local key table exhausted");
    if let Some(f) = dtor {
        unsafe { register_dtor(key, f) };
    }
    key
}

#[inline]
pub unsafe fn set(key: Key, value: *mut u8) {
    debug_assert!(key >= 1 && key < MAX_KEYS);
    tls_table()[key] = value;
}

#[inline]
pub unsafe fn get(key: Key) -> *mut u8 {
    debug_assert!(key >= 1 && key < MAX_KEYS);
    tls_table()[key]
}

#[inline]
pub unsafe fn destroy(_key: Key) {
    // Same posture as Xous: leak the key index. `MAX_KEYS` bounds the
    // lifetime cost, and this bring-up runtime does not create unbounded
    // numbers of distinct `thread_local!` statics.
}

struct Node {
    dtor: Dtor,
    key: Key,
    next: *mut Node,
}

unsafe fn register_dtor(key: Key, dtor: Dtor) {
    // System allocator, to avoid interfering with a potential Global
    // allocator that itself uses thread-local storage.
    let layout = Layout::new::<Node>();
    let node = unsafe { alloc::GlobalAlloc::alloc(&System, layout) } as *mut Node;
    if node.is_null() {
        alloc::handle_alloc_error(layout);
    }
    unsafe { node.write(Node { key, dtor, next: ptr::null_mut() }) };
    let mut head = DTORS.load(Acquire);
    loop {
        unsafe { (*node).next = head };
        match DTORS.compare_exchange(head, node, Release, Acquire) {
            Ok(_) => return,
            Err(cur) => head = cur,
        }
    }
}

/// Runs every registered destructor whose slot on this thread is non-null,
/// then frees this thread's own TLS table — called from this thread's own
/// trampoline (`sys/thread/ask.rs`) just before it exits, mirroring
/// `sys/thread/xous.rs`'s identical call to its own `destroy_tls`. ask has
/// no OS-provided automatic destructor callback to hang this off instead
/// (`sys/thread_local/guard/mod.rs`'s ask arm has the full reasoning).
pub(crate) unsafe fn destroy_tls() {
    let table = table_ptr_addr();
    if table.is_null() {
        return;
    }

    let mut any_run = true;
    for _ in 0..5 {
        if !any_run {
            break;
        }
        any_run = false;
        let mut cur = DTORS.load(Acquire);
        while !cur.is_null() {
            let ptr = unsafe { get((*cur).key) };
            if !ptr.is_null() {
                unsafe { set((*cur).key, ptr::null_mut()) };
                unsafe { ((*cur).dtor)(ptr) };
                any_run = true;
            }
            cur = unsafe { (*cur).next };
        }
    }

    // Safety: `table` was allocated with exactly this layout in
    // `tls_table_slow`, and this thread is about to exit — no further
    // `get`/`set` call on this thread can observe it after this point.
    unsafe { alloc::GlobalAlloc::dealloc(&System, table as *mut u8, table_layout()) };
}
