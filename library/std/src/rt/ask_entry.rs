//! ASK process entry: the runtime drives descriptor staging and the GO handshake
//! and calls back into language initialization, the application and cleanup.

use core::ffi::c_void;

use super::*;

struct Entry<'a> {
    main: &'a (dyn Fn() -> i32 + Sync + crate::panic::RefUnwindSafe),
    argc: isize,
    argv: *const *const u8,
    sigpipe: u8,
}

unsafe extern "C" fn initialize(context: *mut c_void) {
    let entry = unsafe { &*context.cast::<Entry<'_>>() };
    panic::catch_unwind(|| unsafe { init(entry.argc, entry.argv, entry.sigpipe) })
        .unwrap_or_else(handle_rt_panic);
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
    let record = askrust::rt::StartRecord {
        context: core::ptr::from_mut(&mut entry).cast(),
        initialize,
        main: application,
        cleanup: finish,
    };
    // SAFETY: the callbacks accept `entry`, which outlives the call.
    match unsafe { askrust::rt::start(&record) } {
        Ok(code) => code as isize,
        Err(_) => {
            askrust::rt::log("process entry refused before application completion");
            askrust::rt::exit(127)
        }
    }
}
