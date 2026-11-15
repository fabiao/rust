//! System allocator for ASK: the runtime's process heap.

use crate::alloc::Layout;

pub fn reclaim(retain_bytes: usize) -> core::alloc::ReclaimResult {
    if askrust::alloc::reclaim(retain_bytes) {
        core::alloc::ReclaimResult::Released
    } else {
        core::alloc::ReclaimResult::Unchanged
    }
}

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    // SAFETY: the preconditions match `GlobalAlloc::alloc`.
    unsafe { askrust::alloc::alloc(layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: the preconditions match `GlobalAlloc::alloc_zeroed`.
    unsafe { askrust::alloc::alloc_zeroed(layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: the caller allocated `ptr` with `layout` from this heap.
    unsafe { askrust::alloc::dealloc(ptr, layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: the caller allocated `ptr` with `layout` from this heap.
    unsafe { askrust::alloc::realloc(ptr, layout.size(), layout.align(), new_size) }
}
