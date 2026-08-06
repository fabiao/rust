//! System allocator for ASK, backed directly by the canonical `askabi`
//! `Map` syscall wrapper. The allocator policy belongs to the std PAL;
//! `askabi` remains a dependency-free description of native mechanisms.

use crate::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const HEAP_BASE: u64 = 0x0000_3800_0000_0000;
const CHUNK_LEN: u64 = 64 * 4096;

struct BumpState {
    next: AtomicU64,
    end: AtomicU64,
}

static STATE: BumpState = BumpState {
    next: AtomicU64::new(HEAP_BASE),
    end: AtomicU64::new(HEAP_BASE),
};
static LOCK: AtomicBool = AtomicBool::new(false);

fn lock() {
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn unlock() {
    LOCK.store(false, Ordering::Release);
}

fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    let align = (layout.align() as u64).max(1);
    let size = layout.size() as u64;

    lock();
    let next = STATE.next.load(Ordering::Relaxed);
    let mut end = STATE.end.load(Ordering::Relaxed);
    let mut aligned = align_up(next, align);

    if aligned.checked_add(size).is_none_or(|top| top > end) {
        let grow = align_up(size.max(CHUNK_LEN), 4096);
        if ask_abi::map(end, grow, true, false, ask_abi::APP_FRAME_TOKEN).is_err() {
            unlock();
            return core::ptr::null_mut();
        }
        end += grow;
        STATE.end.store(end, Ordering::Relaxed);
        aligned = align_up(next, align);
    }

    STATE.next.store(aligned + size, Ordering::Relaxed);
    unlock();
    core::ptr::with_exposed_provenance_mut(aligned as usize)
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: forwards the caller's valid layout to this module's allocator.
    let ptr = unsafe { alloc(layout) };
    if !ptr.is_null() {
        // SAFETY: `alloc` returned a region valid for `layout.size()` bytes.
        unsafe { core::ptr::write_bytes(ptr, 0, layout.size()) };
    }
    ptr
}

#[inline]
pub unsafe fn dealloc(_ptr: *mut u8, _layout: Layout) {
    // Bring-up allocator: mappings are process-owned and reclaimed at exit.
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `new_layout` was validated above.
    let new_ptr = unsafe { alloc(new_layout) };
    if !new_ptr.is_null() {
        // SAFETY: the caller guarantees the old allocation, and `new_ptr`
        // is valid for at least the copied byte count.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
        }
    }
    new_ptr
}
