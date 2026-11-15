//! Standard streams capture the current resident descriptor for each operation.

use crate::io;
use crate::sys::pal::ask_error;

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;
pub struct Stdin;
pub struct Stdout;
pub struct Stderr;
impl Stdin {
    pub const fn new() -> Self {
        Self
    }
}
impl Stdout {
    pub const fn new() -> Self {
        Self
    }
}
impl Stderr {
    pub const fn new() -> Self {
        Self
    }
}

use askrust::fd::number::{STDERR, STDIN, STDOUT};

impl io::Read for Stdin {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        askrust::fd::read(STDIN, bytes).map_err(ask_error)
    }
    fn read_buf(&mut self, cursor: io::BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|bytes| askrust::fd::read(STDIN, bytes).map_err(ask_error), cursor)
    }
}
impl io::Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        askrust::fd::write(STDOUT, bytes).map_err(ask_error)
    }
    fn flush(&mut self) -> io::Result<()> {
        askrust::fd::flush(STDOUT).map_err(ask_error)
    }
}
impl io::Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        askrust::fd::write(STDERR, bytes).map_err(ask_error)
    }
    fn flush(&mut self) -> io::Result<()> {
        askrust::fd::flush(STDERR).map_err(ask_error)
    }
}
pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr)
}
pub fn is_ebadf(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
}
