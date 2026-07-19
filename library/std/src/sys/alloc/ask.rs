//! System allocator for ask, backed by `ask-abi`'s `Map`-based heap (the
//! same mechanism `libask`'s service heap uses). Free-function shape,
//! following the Motor OS port (`motor.rs`).

use crate::alloc::Layout;

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    // SAFETY: same requirements as GlobalAlloc::alloc.
    unsafe { ask_abi::alloc::alloc(layout) }
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: same requirements as GlobalAlloc::alloc_zeroed.
    unsafe { ask_abi::alloc::alloc_zeroed(layout) }
}

#[inline]
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: same requirements as GlobalAlloc::dealloc.
    unsafe { ask_abi::alloc::dealloc(ptr, layout) }
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: same requirements as GlobalAlloc::realloc.
    unsafe { ask_abi::alloc::realloc(ptr, layout, new_size) }
}
