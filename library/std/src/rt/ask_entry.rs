use core::ffi::c_void;

use ask_fd::{ErrorRecord, Init, ProcessEntry, error_domain, execution_class, owner_error, status};

use super::*;
use crate::alloc::{GlobalAlloc, Layout, System};

pub(crate) fn execution_class() -> u32 {
    unsafe extern "C" {
        static __ask_entry_record: [u8; ask_io::entry::Record::SIZE];
    }
    let record = unsafe { &__ask_entry_record };
    match ask_io::entry::Record::decode(record) {
        Ok(record) => record.class,
        Err(_) => crate::sys::abort_internal(),
    }
}

struct Entry<'a> {
    main: &'a (dyn Fn() -> i32 + Sync + crate::panic::RefUnwindSafe),
    argc: isize,
    argv: *const *const u8,
    sigpipe: u8,
}

unsafe extern "C" fn allocate(
    _: *mut c_void,
    size: u64,
    alignment: u64,
    output: *mut *mut c_void,
    error: *mut ErrorRecord,
) -> u32 {
    unsafe {
        output.write(core::ptr::null_mut());
        error.write(ErrorRecord::clear());
    }
    let layout = usize::try_from(size)
        .ok()
        .zip(usize::try_from(alignment).ok())
        .and_then(|(size, alignment)| Layout::from_size_align(size, alignment).ok())
        .filter(|layout| layout.size() != 0);
    let Some(layout) = layout else {
        unsafe {
            error.write(ErrorRecord {
                domain: error_domain::OWNER,
                native_code: owner_error::INVALID_ARGUMENT,
                detail: size,
            })
        };
        return status::RANGE;
    };
    // System is the resident primitive allocator; no application allocator callback runs here.
    let allocation = unsafe { System.alloc(layout) };
    if allocation.is_null() {
        unsafe {
            error.write(ErrorRecord { domain: error_domain::MEMORY, native_code: 0, detail: size })
        };
        return status::BACKING;
    }
    unsafe { output.write(allocation.cast()) };
    status::COMPLETE
}

unsafe extern "C" fn release(_: *mut c_void, pointer: *mut c_void, size: u64, alignment: u64) {
    let layout = usize::try_from(size)
        .ok()
        .zip(usize::try_from(alignment).ok())
        .and_then(|(size, alignment)| Layout::from_size_align(size, alignment).ok());
    let Some(layout) = layout else {
        crate::sys::abort_internal();
    };
    unsafe { System.dealloc(pointer.cast(), layout) };
}

unsafe extern "C" fn initialize(context: *mut c_void, error: *mut ErrorRecord) -> u32 {
    let entry = unsafe { &*context.cast::<Entry<'_>>() };
    panic::catch_unwind(|| unsafe { init(entry.argc, entry.argv, entry.sigpipe) })
        .unwrap_or_else(handle_rt_panic);
    unsafe { error.write(ErrorRecord::clear()) };
    status::COMPLETE
}

unsafe extern "C" fn application(context: *mut c_void) -> i32 {
    let entry = unsafe { &*context.cast::<Entry<'_>>() };
    panic::catch_unwind(entry.main).unwrap_or_else(|payload| {
        let payload = panic::AssertUnwindSafe(payload);
        panic::catch_unwind(move || drop({ payload }.0)).unwrap_or_else(move |error| {
            mem::forget(error);
            rtabort!("drop of the panic payload panicked");
        });
        101
    })
}

unsafe extern "C" fn finish(_: *mut c_void) {
    panic::catch_unwind(|| {
        cleanup();
        crate::sys::exit::unique_thread_exit();
    })
    .unwrap_or_else(handle_rt_panic);
}

pub(super) fn run(
    main: &(dyn Fn() -> i32 + Sync + crate::panic::RefUnwindSafe),
    argc: isize,
    argv: *const *const u8,
    sigpipe: u8,
) -> isize {
    let mut entry = Entry { main, argc, argv, sigpipe };
    let init = Init {
        owning_thread: ask_sys::get_tid() as u64,
        execution_class: execution_class(),
        reserved: 0,
        allocate,
        release,
        allocator_context: core::ptr::null_mut(),
    };
    let input = ProcessEntry {
        owner_init: &init,
        initialize_user: initialize,
        synchronous_main: if init.execution_class == execution_class::CLASS1 {
            core::ptr::null()
        } else {
            (application as *const ()).cast()
        },
        cleanup_user: finish,
        context: core::ptr::from_mut(&mut entry).cast(),
    };
    let mut exit_code = 0;
    let mut error = ErrorRecord::clear();
    let result = unsafe { ask_fd::__ask_process_entry(&input, &mut exit_code, &mut error) };
    if result != status::COMPLETE {
        ask_sys::log("process entry refused before application completion");
        ask_sys::exit(127);
    }
    exit_code as isize
}
