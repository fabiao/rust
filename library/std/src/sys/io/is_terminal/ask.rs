//! `IsTerminal` for ask: stdio tracks the controlling-terminal lease.
//! Ordinary files are never terminals; ASK has no fd-based `isatty`.

pub trait PalIsTerminal {
    fn pal_is_terminal(&self) -> bool {
        false
    }
}

impl PalIsTerminal for crate::fs::File {}

impl PalIsTerminal for crate::io::Stdin {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

impl PalIsTerminal for crate::io::Stdout {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

impl PalIsTerminal for crate::io::Stderr {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

impl PalIsTerminal for crate::io::StdinLock<'_> {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

impl PalIsTerminal for crate::io::StdoutLock<'_> {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

impl PalIsTerminal for crate::io::StderrLock<'_> {
    fn pal_is_terminal(&self) -> bool {
        crate::sys::stdio::has_controlling_terminal()
    }
}

pub fn is_terminal<T: PalIsTerminal>(value: &T) -> bool {
    value.pal_is_terminal()
}
