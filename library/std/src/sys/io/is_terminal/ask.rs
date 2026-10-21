//! Terminal identity is queried from the captured resident description.

pub trait PalIsTerminal {
    fn pal_is_terminal(&self) -> bool;
}
impl PalIsTerminal for crate::fs::File {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::fd::is_terminal(crate::os::fd::AsRawFd::as_raw_fd(self))
    }
}
impl PalIsTerminal for crate::os::fd::OwnedFd {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::fd::is_terminal(crate::os::fd::AsRawFd::as_raw_fd(self))
    }
}
impl PalIsTerminal for crate::os::fd::BorrowedFd<'_> {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::fd::is_terminal(crate::os::fd::AsRawFd::as_raw_fd(self))
    }
}
macro_rules! standard_stream {
    ($type:ty, $number:expr) => {
        impl PalIsTerminal for $type {
            fn pal_is_terminal(&self) -> bool {
                crate::sys::fd::is_terminal($number)
            }
        }
    };
}
standard_stream!(crate::io::Stdin, ask_io::fd::number::STDIN);
standard_stream!(crate::io::Stdout, ask_io::fd::number::STDOUT);
standard_stream!(crate::io::Stderr, ask_io::fd::number::STDERR);
standard_stream!(crate::io::StdinLock<'_>, ask_io::fd::number::STDIN);
standard_stream!(crate::io::StdoutLock<'_>, ask_io::fd::number::STDOUT);
standard_stream!(crate::io::StderrLock<'_>, ask_io::fd::number::STDERR);
pub fn is_terminal<T: PalIsTerminal>(value: &T) -> bool {
    value.pal_is_terminal()
}
