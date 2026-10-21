//! Standard streams capture the current resident descriptor for each operation.

use crate::io;
use crate::sys::fd::ask::{blocking, perform, request};

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

fn read(number: i32, bytes: &mut [u8]) -> io::Result<usize> {
    let result =
        blocking(perform(number, request(ask_fd::action::READ, bytes.len()), &[], bytes))??;
    usize::try_from(result.completed_count).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}
fn write(number: i32, bytes: &[u8]) -> io::Result<usize> {
    let result =
        blocking(perform(number, request(ask_fd::action::WRITE, bytes.len()), bytes, &mut []))??;
    usize::try_from(result.completed_count).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}
fn flush(number: i32) -> io::Result<()> {
    blocking(perform(number, request(ask_fd::action::FLUSH, 0), &[], &mut []))?.map(|_| ())
}
impl io::Read for Stdin {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        read(ask_io::fd::number::STDIN, bytes)
    }
    fn read_buf(&mut self, cursor: io::BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|bytes| read(ask_io::fd::number::STDIN, bytes), cursor)
    }
}
impl io::Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        write(ask_io::fd::number::STDOUT, bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        flush(ask_io::fd::number::STDOUT)
    }
}
impl io::Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        write(ask_io::fd::number::STDERR, bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        flush(ask_io::fd::number::STDERR)
    }
}
pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr)
}
pub fn is_ebadf(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
}
