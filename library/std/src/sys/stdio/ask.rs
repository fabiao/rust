use crate::io;

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        // No stdin ring exists yet (docs/02's open stdio-convention
        // question) — the first PAL slice routes stdout/stderr through
        // `Log` only (docs/10-rust-toolchain.md's decided bring-up posture).
        crate::sys::pal::unsupported()
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_log(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

impl io::Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_log(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `Log` is a whole-line syscall with no fd distinction (`kernel/src/syscall/mod.rs`), so stdout
/// and stderr both land on the same serial sink for now — matching every existing service's
/// bring-up posture, not a stdio design decision.
fn write_log(buf: &[u8]) -> io::Result<usize> {
    match core::str::from_utf8(buf) {
        Ok(s) => {
            ask_abi::log(s);
            Ok(buf.len())
        }
        Err(e) => {
            let valid = e.valid_up_to();
            if valid == 0 {
                return Err(io::const_error!(io::ErrorKind::InvalidData, "stdio: invalid UTF-8"));
            }
            // Safety: `valid` bytes were just reported valid by `from_utf8`.
            ask_abi::log(unsafe { core::str::from_utf8_unchecked(&buf[..valid]) });
            Ok(valid)
        }
    }
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}
