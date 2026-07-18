//! Raw syscall ABI (docs/02-kernel-abi.md): id in `rax`, args in
//! `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`, result in `rax`. The dependency-free
//! surface both `libask` and the std PAL (`sys/ask`) build on.
//! Details: docs/src/ask-abi/src/lib.md.
#![no_std]

/// std-PAL-only addition (`sys/alloc/ask.rs`'s backing allocator) — not
/// present in the superproject's canonical `ask-abi/` crate, which the
/// kernel and `libask` (already `linked_list_allocator`-backed) also
/// depend on and which this module has no place in.
pub mod alloc;

pub const SYS_LOG: u64 = 0;
pub const SYS_EXIT: u64 = 1;
pub const SYS_SPAWN_RAW: u64 = 2;
pub const SYS_YIELD: u64 = 3;
pub const SYS_MAP: u64 = 4;
pub const SYS_REVOKE: u64 = 5;
pub const SYS_GRANT: u64 = 6;
pub const SYS_CHANNEL_CREATE: u64 = 7;
pub const SYS_CHANNEL_ACCEPT: u64 = 8;
pub const SYS_PARK: u64 = 9;
pub const SYS_WAKE: u64 = 10;
pub const SYS_MAP_PHYSICAL: u64 = 11;
pub const SYS_MAP_IO_PORT: u64 = 12;
pub const SYS_BIND_INTERRUPT: u64 = 13;
pub const SYS_GET_RSDP: u64 = 14;
pub const SYS_TRANSLATE: u64 = 15;
pub const SYS_MAP_CONTIGUOUS: u64 = 16;
pub const SYS_GET_PARENT_PID: u64 = 17;
pub const SYS_PARK_TIMEOUT: u64 = 18;
pub const SYS_GET_PID: u64 = 19;
pub const SYS_GET_COREFS_MANIFEST: u64 = 20;

/// Cap on a single `Log` payload — matches the kernel's own `LOG_MAX`
/// (`kernel/src/syscall/mod.rs`).
pub const LOG_MAX: usize = 256;

/// Kernel error values, returned in `rax`. The discriminants sit at the top
/// of the `u64` range so they never collide with a legitimate small-integer
/// success value (e.g. a `SpawnRaw` pid). The kernel's dispatcher and every
/// userspace caller import this same definition — nothing is hand-mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Error {
    Denied = u64::MAX,
    InvalidParams = u64::MAX - 1,
    UnknownSyscall = u64::MAX - 2,
}

/// Every legitimate return value from these syscalls (a pid, a virtual
/// address below `USER_ADDR_LIMIT`) sits far below the `u64::MAX`-adjacent
/// sentinel range the kernel's `Error` enum occupies.
fn decode(v: u64) -> Result<u64, Error> {
    match v {
        v if v == u64::MAX => Err(Error::Denied),
        v if v == u64::MAX - 1 => Err(Error::InvalidParams),
        v if v == u64::MAX - 2 => Err(Error::UnknownSyscall),
        v => Ok(v),
    }
}

/// # Safety
/// Any pointer argument must be valid for the kernel's synchronous access
/// during the call.
unsafe fn syscall2(id: u64, a0: u64, a1: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") id => ret,
            in("rdi") a0,
            in("rsi") a1,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// # Safety
/// Any pointer argument must be valid for the kernel's synchronous access
/// during the call.
unsafe fn syscall3(id: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") id => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// # Safety
/// Any pointer argument must be valid for the kernel's synchronous access
/// during the call.
unsafe fn syscall4(id: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") id => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// `Log(ptr, len)`: print a UTF-8 string, clamped to `LOG_MAX`. Prefer
/// `libask`'s `logln!` (or the std PAL's stdout) over calling this directly.
pub fn log(msg: &str) {
    // Clamp on a char boundary — slicing a multi-byte char in half would
    // itself panic.
    let mut end = msg.len().min(LOG_MAX);
    while end > 0 && !msg.is_char_boundary(end) {
        end -= 1;
    }
    let msg = msg.get(..end).unwrap_or("");
    // Safety: `msg` is a valid, bounded slice for the duration of the call.
    unsafe { syscall2(SYS_LOG, msg.as_ptr() as u64, msg.len() as u64) };
}

/// `Exit(code)`: end this process. Never returns.
pub fn exit(code: u64) -> ! {
    // Retrying (rather than `unreachable!`) keeps the impossible
    // Exit-returned case from panicking inside the exit path itself.
    loop {
        // Safety: `SYS_EXIT` never returns.
        unsafe { syscall2(SYS_EXIT, code, 0) };
    }
}

/// `SpawnRaw(block_ptr, block_len)`: spawn from a binary capability block.
/// Untyped here — the `CapabilityBlock` struct is `libask`'s; use
/// `libask::syscall::spawn_raw` instead of calling this directly.
/// # Safety
/// `[block_ptr, block_ptr+block_len)` must be a valid, live capability
/// block the kernel can read synchronously during the call.
pub unsafe fn spawn_raw(block_ptr: *const u8, block_len: usize) -> Result<usize, Error> {
    decode(unsafe { syscall2(SYS_SPAWN_RAW, block_ptr as u64, block_len as u64) })
        .map(|v| v as usize)
}

/// `Yield()`: give up the core cooperatively.
pub fn yield_now() {
    // Safety: `Yield` takes no pointer arguments.
    unsafe { syscall2(SYS_YIELD, 0, 0) };
}

/// `Map(virt, len, writable)`: allocate and map fresh anonymous memory.
pub fn map(virt: u64, len: u64, writable: bool) -> Result<(), Error> {
    // Safety: caller-provided range; the kernel validates it against
    // `USER_ADDR_LIMIT` before touching any mapping.
    decode(unsafe { syscall3(SYS_MAP, virt, len, writable as u64) }).map(|_| ())
}

/// `Revoke(virt, len)`: unmap this process's own mapping.
pub fn revoke(virt: u64, len: u64) -> Result<(), Error> {
    // Safety: unmaps only pages this process mapped itself.
    decode(unsafe { syscall2(SYS_REVOKE, virt, len) }).map(|_| ())
}

/// `Grant(target_pid, virt, len, dest_virt)`: share this process's mapping
/// into another address space.
pub fn grant(target_pid: u64, virt: u64, len: u64, dest_virt: u64) -> Result<(), Error> {
    // Safety: shares a page this process mapped and, by convention, has
    // already finished writing to before granting it away.
    decode(unsafe { syscall4(SYS_GRANT, target_pid, virt, len, dest_virt) }).map(|_| ())
}

/// `ChannelCreate(target_pid, pages)`: establish a shared-memory channel,
/// returning this side's local virtual address for it.
pub fn channel_create(target_pid: u64, pages: u64) -> Result<u64, Error> {
    // Safety: `target_pid` must name a live process.
    decode(unsafe { syscall2(SYS_CHANNEL_CREATE, target_pid, pages) })
}

/// Claim the oldest pending channel, returning its local mapping and the
/// kernel-recorded depositor pid.
pub fn channel_accept() -> Result<(u64, usize), Error> {
    let mut depositor_pid: u64 = 0;
    // Safety: `&mut depositor_pid` is a valid, writable pointer into this
    // process's own stack.
    let virt = decode(unsafe {
        syscall2(SYS_CHANNEL_ACCEPT, &mut depositor_pid as *mut u64 as u64, 0)
    })?;
    Ok((virt, depositor_pid as usize))
}

/// `Park()`: give up the core until another process calls `Wake` on us.
pub fn park() {
    // Safety: `Park` takes no pointer arguments.
    unsafe { syscall2(SYS_PARK, 0, 0) };
}

/// `ParkTimeout(ms)`: `park`, but the kernel wakes us on its own after `ms`
/// milliseconds if nobody `Wake`s us first (docs/04-scheduling.md's
/// Park-with-deadline primitive). Returns `true` if the deadline is what
/// woke us (a real timeout), `false` if an explicit `Wake` did.
pub fn park_timeout(ms: u64) -> bool {
    // Safety: `ParkTimeout` takes no pointer arguments.
    unsafe { syscall2(SYS_PARK_TIMEOUT, ms, 0) != 0 }
}

/// `Wake(target_pid)`: make `target_pid` schedulable again if it's parked.
/// A no-op, not an error, if the target isn't currently parked (kernel
/// semantics, `kernel/src/proc/mod.rs`'s `wake`) — safe to call
/// unconditionally after posting to a ring.
pub fn wake(target_pid: u64) -> Result<(), Error> {
    // Safety: `Wake` takes no pointer arguments beyond the target pid.
    decode(unsafe { syscall2(SYS_WAKE, target_pid, 0) }).map(|_| ())
}

/// `MapPhysical(virt, phys, len, writable)`: map a fixed physical range into
/// this process's own address space (docs/12-device-management.md).
/// Requires a `Capability::MapPhysical` token covering `[phys, phys+len)`.
pub fn map_physical(virt: u64, phys: u64, len: u64, writable: bool) -> Result<(), Error> {
    // Safety: caller-provided ranges; the kernel validates `virt` against
    // `USER_ADDR_LIMIT` and `[phys, phys+len)` against the caller's
    // `MapPhysical` capability before touching any mapping.
    decode(unsafe { syscall4(SYS_MAP_PHYSICAL, virt, phys, len, writable as u64) }).map(|_| ())
}

/// `MapIoPort(port, len)`: activate `[port, port+len)` for direct Ring 3
/// `in`/`out` access (docs/12-device-management.md). Requires a
/// `Capability::IoPort` token covering the same range.
pub fn map_io_port(port: u16, len: u16) -> Result<(), Error> {
    // Safety: `MapIoPort` takes no pointer arguments.
    decode(unsafe { syscall2(SYS_MAP_IO_PORT, port as u64, len as u64) }).map(|_| ())
}

/// Bind `gsi`, returning the shared counter-page address; the page also
/// exposes the kernel-allocated IDT vector.
pub fn bind_interrupt(gsi: u32) -> Result<u64, Error> {
    // Safety: `BindInterrupt` takes no pointer arguments.
    decode(unsafe { syscall2(SYS_BIND_INTERRUPT, gsi as u64, 0) })
}

/// `GetRsdp()`: the RSDP's physical address, if the bootloader reported one
/// (docs/12-device-management.md). No capability is required to learn this
/// bare number — only actually mapping it (`map_physical`) is gated.
pub fn get_rsdp() -> Result<u64, Error> {
    // Safety: `GetRsdp` takes no pointer arguments.
    decode(unsafe { syscall2(SYS_GET_RSDP, 0, 0) })
}

/// Return the physical address backing the caller's mapped `virt`, allowing
/// userspace drivers to program DMA-capable devices.
pub fn translate(virt: u64) -> Result<u64, Error> {
    // Safety: `Translate` takes no pointer arguments beyond the address
    // itself, which the kernel validates against `USER_ADDR_LIMIT` and
    // against whether anything is actually mapped there.
    decode(unsafe { syscall2(SYS_TRANSLATE, virt, 0) })
}

/// `MapContiguous(virt, len, writable)`: like `map`, but the underlying
/// frames are guaranteed physically contiguous (docs/12-device-management.md)
/// — needed by a DMA-capable device addressing a multi-page buffer as one
/// linear physical range. Requires `Capability::Map`, same as `map`.
pub fn map_contiguous(virt: u64, len: u64, writable: bool) -> Result<(), Error> {
    // Safety: caller-provided range; the kernel validates it against
    // `USER_ADDR_LIMIT` before touching any mapping.
    decode(unsafe { syscall3(SYS_MAP_CONTIGUOUS, virt, len, writable as u64) }).map(|_| ())
}

/// `GetParentPid()`: the pid that spawned this process.
pub fn get_parent_pid() -> usize {
    // Safety: `GetParentPid` takes no pointer arguments and cannot fail.
    unsafe { syscall2(SYS_GET_PARENT_PID, 0, 0) as usize }
}

/// `GetPid()`: this process's own pid, straight from Ring 0 on every call.
/// `libask::syscall::get_pid` memoizes this; a std PAL caches it its own way.
pub fn get_pid_uncached() -> usize {
    // Safety: `GetPid` takes no pointer arguments and cannot fail.
    unsafe { syscall2(SYS_GET_PID, 0, 0) as usize }
}

/// `GetCorefsManifest(ptr, len)`: copy the signed boot manifest into an
/// exactly sized writable buffer.
pub fn get_corefs_manifest(output: &mut [u8]) -> Result<(), Error> {
    // Safety: `output` is a valid, writable slice for the duration of the
    // call.
    decode(unsafe {
        syscall2(
            SYS_GET_COREFS_MANIFEST,
            output.as_mut_ptr() as u64,
            output.len() as u64,
        )
    })
    .map(|_| ())
}
