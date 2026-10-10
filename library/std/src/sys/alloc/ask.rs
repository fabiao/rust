//! System allocator for ASK: dlmalloc over `askalloc` segments, which the
//! kernel places (`Map` with `ANYWHERE`) and revokes when dlmalloc frees or
//! trims them. Adjacent backing allocations remain separately reclaimable.
//! The futex mutex parks a contending thread while the owner runs.

use core::cell::SyncUnsafeCell;

use crate::alloc::Layout;
use crate::sys::sync::Mutex;

type Heap = ask_alloc::Dlmalloc<ask_alloc::Segments<ask_alloc::KernelPages>>;

struct SyncHeap(Heap);
// SAFETY: every access to the heap holds `LOCK`.
unsafe impl Sync for SyncHeap {}

static HEAP: SyncUnsafeCell<SyncHeap> = SyncUnsafeCell::new(SyncHeap(
    ask_alloc::Dlmalloc::new_with_allocator(ask_alloc::Segments::new(
        ask_alloc::KernelPages::new(ask_abi::APP_FRAME_TOKEN),
    )),
));
static LOCK: Mutex = Mutex::new();

/// Runs `operation` with exclusive access to the heap.
fn with_heap<T>(operation: impl FnOnce(&mut Heap) -> T) -> T {
    LOCK.lock();
    // SAFETY: `LOCK` is held, so this is the only live reference.
    let result = operation(unsafe { &mut (*HEAP.get()).0 });
    // SAFETY: locked above by this thread.
    unsafe { LOCK.unlock() };
    result
}

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    // SAFETY: the preconditions match `GlobalAlloc::alloc`.
    with_heap(|heap| unsafe { heap.malloc(layout.size(), layout.align()) })
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: the preconditions match `GlobalAlloc::alloc_zeroed`.
    with_heap(|heap| unsafe { heap.calloc(layout.size(), layout.align()) })
}

#[inline]
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: the caller allocated `ptr` with `layout` from this heap.
    with_heap(|heap| unsafe { heap.free(ptr, layout.size(), layout.align()) })
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: the caller allocated `ptr` with `layout` from this heap.
    with_heap(|heap| unsafe { heap.realloc(ptr, layout.size(), layout.align(), new_size) })
}
