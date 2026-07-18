//! Minimal global allocator for the std PAL (`sys/alloc/ask.rs`): a
//! growable bump allocator over `Map`-backed pages. Bring-up stand-in —
//! `dealloc` is a no-op (never reclaims) until a real allocator design
//! replaces it; that's an accepted first-slice limitation, not a hidden one.
//! Not part of the superproject's canonical `ask-abi/` (kernel/libask side):
//! this module exists only in the fork's vendored copy, std-PAL-only.

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::map;

/// Fixed virtual base for the std PAL heap — distinct from `libask::heap`'s
/// `HEAP_BASE` (`0x0000_3000_...`) so a mixed binary linking both `libask`
/// and std would never collide; std binaries don't link `libask`, so this
/// is precautionary, not load-bearing today.
const HEAP_BASE: u64 = 0x0000_3800_0000_0000;
/// Growth step, matching `libask::heap`'s fixed budget.
const CHUNK_LEN: u64 = 64 * 4096;

struct BumpState {
    next: AtomicU64,
    end: AtomicU64,
}

static STATE: BumpState = BumpState { next: AtomicU64::new(HEAP_BASE), end: AtomicU64::new(HEAP_BASE) };
static LOCK: AtomicBool = AtomicBool::new(false);

/// Single-core cooperative scheduler today (docs/04-scheduling.md) — a
/// spin lock never actually contends, same posture as every other
/// bring-up synchronization primitive in this codebase.
fn lock() {
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}

fn unlock() {
    LOCK.store(false, Ordering::Release);
}

fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// # Safety
/// Same contract as `GlobalAlloc::alloc`.
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    let align = (layout.align() as u64).max(1);
    let size = layout.size() as u64;

    lock();
    let next = STATE.next.load(Ordering::Relaxed);
    let mut end = STATE.end.load(Ordering::Relaxed);
    let mut aligned = align_up(next, align);

    if aligned.checked_add(size).is_none_or(|top| top > end) {
        let grow = align_up(size.max(CHUNK_LEN), 4096);
        if map(end, grow, true).is_err() {
            unlock();
            return core::ptr::null_mut();
        }
        end += grow;
        STATE.end.store(end, Ordering::Relaxed);
        aligned = align_up(next, align);
    }

    STATE.next.store(aligned + size, Ordering::Relaxed);
    unlock();
    aligned as *mut u8
}

/// # Safety
/// Same contract as `GlobalAlloc::alloc_zeroed`.
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // Safety: forwarding the same layout to `alloc`.
    let ptr = unsafe { alloc(layout) };
    if !ptr.is_null() {
        // Safety: `alloc` just returned a valid `layout.size()`-byte region.
        unsafe { core::ptr::write_bytes(ptr, 0, layout.size()) };
    }
    ptr
}

/// # Safety
/// Same contract as `GlobalAlloc::dealloc`.
pub unsafe fn dealloc(_ptr: *mut u8, _layout: Layout) {
    // Bump allocator: no reclamation (see module doc). Proves alloc-dependent
    // std code paths end to end; a real allocator lands before Phase 7 is
    // considered done (docs/10-rust-toolchain.md).
}

/// # Safety
/// Same contract as `GlobalAlloc::realloc`.
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
        return core::ptr::null_mut();
    };
    // Safety: `new_layout` is valid (just constructed above).
    let new_ptr = unsafe { alloc(new_layout) };
    if !new_ptr.is_null() {
        let copy = layout.size().min(new_size);
        // Safety: `ptr` is valid for `layout.size()` bytes (caller contract);
        // `new_ptr` is valid for `new_size >= copy` bytes (just allocated).
        unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy) };
    }
    new_ptr
}
