//! `std::io`'s standard streams on ask. A signed application launched with a
//! controlling terminal receives its endpoint as a generation-bound Channel
//! lease at `APP_TERMINAL_TOKEN` (`ask_abi::capability`); this module adopts
//! that endpoint on first use so `stdin` reads real terminal input and
//! `stdout`/`stderr` reach the same terminal rather than the serial log.
//!
//! Without that lease — a service, a driver, or any process Launcher gave no
//! terminal — the streams fall back to the whole-line `Log` syscall for
//! output, and `stdin` reports end-of-file rather than failing, matching what
//! a process with no controlling terminal sees elsewhere.

use crate::ffi::OsStr;
use crate::io;
use crate::sync::Mutex;
use crate::sys::channel::SyncChannel;
use crate::sys::env;
use crate::sys::pipe::{self, Pipe};

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;

/// The process-wide controlling terminal, adopted once. `None` once an
/// attempt has established there is no terminal lease, so a terminal-less
/// process pays one failed `ChannelCreate` rather than one per write.
static TERMINAL: Mutex<TerminalState> = Mutex::new(TerminalState::Unattached);
static STDIN_PIPE: Mutex<Option<Pipe>> = Mutex::new(None);
static STDOUT_PIPE: Mutex<Option<Pipe>> = Mutex::new(None);
static STDERR_PIPE: Mutex<Option<Pipe>> = Mutex::new(None);

enum TerminalState {
    Unattached,
    Absent,
    Attached(SyncChannel),
}

/// Adopt Command-redirected stdio pipes before user `main`. Parent and child
/// agree on create/accept order: stdout, then stderr, then stdin accept.
pub fn adopt_command_stdio() {
    if env::getenv(OsStr::new("ASK_STDIN_PIPE")).is_none() {
        pipe::discard_unclaimed_channels();
    }
    if env::getenv(OsStr::new("ASK_STDOUT_PIPE")).is_some() {
        match pipe::writer_to_endpoint(ask_abi::app_stdio_endpoint::PARENT) {
            Ok(pipe) => {
                *STDOUT_PIPE.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
            }
            Err(_) => {
                ask_sys::log("stdio: stdout pipe create failed");
            }
        }
    }
    if env::getenv(OsStr::new("ASK_STDERR_PIPE")).is_some() {
        match pipe::writer_to_endpoint(ask_abi::app_stdio_endpoint::PARENT) {
            Ok(pipe) => {
                *STDERR_PIPE.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
            }
            Err(_) => {
                ask_sys::log("stdio: stderr pipe create failed");
            }
        }
    }
    if env::getenv(OsStr::new("ASK_STDIN_PIPE")).is_some() {
        if let Ok(pipe) = pipe::accept_reader() {
            *STDIN_PIPE.lock().unwrap_or_else(|e| e.into_inner()) = Some(pipe);
        }
    }
}

/// True once this process has adopted a controlling-terminal Channel lease.
pub fn has_controlling_terminal() -> bool {
    with_terminal(|_| Ok(())).is_some()
}

/// Run `f` against the controlling terminal, attaching it on first use.
/// Returns `None` when this process has no terminal lease.
fn with_terminal<T>(f: impl FnOnce(&mut SyncChannel) -> io::Result<T>) -> Option<io::Result<T>> {
    let mut state = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
    if let TerminalState::Unattached = *state {
        *state = match SyncChannel::create_leased(
            ask_abi::APP_TERMINAL_TOKEN,
            ask_io::terminal::CHANNEL_PAGES,
        ) {
            Ok(channel) => TerminalState::Attached(channel),
            Err(_) => TerminalState::Absent,
        };
    }
    match *state {
        TerminalState::Attached(ref mut channel) => Some(f(channel)),
        _ => None,
    }
}

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(pipe) = STDIN_PIPE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return pipe.read(buf);
        }
        // No controlling terminal is end-of-file, not an error — the same
        // thing a process reading a closed stdin observes.
        with_terminal(|channel| terminal_read(channel, buf)).unwrap_or(Ok(0))
    }
}

/// One `OP_READ` round trip. The reply's bytes land in the channel's shared
/// data window, described by the completion's `TerminalBuffer`.
fn terminal_read(channel: &mut SyncChannel, buf: &mut [u8]) -> io::Result<usize> {
    let completion = channel.call(ask_io::terminal::OP_READ, &[])?;
    match completion.result {
        // The master closed its direction: end of input, not a failure.
        ask_io::terminal::RESULT_EOF => return Ok(0),
        result if result < 0 => {
            return Err(io::const_error!(
                io::ErrorKind::Other,
                "stdin: terminal read failed"
            ));
        }
        _ => {}
    }
    let descriptor = ask_io::terminal::decode_terminal_send_request(completion.payload())
        .ok_or_else(crate::sys::pal::unsupported_err)?;
    let len = (descriptor.len as usize).min(buf.len());
    let start = ask_io::terminal::DATA_OFFSET as usize + descriptor.offset as usize;
    let window = channel
        .shared_region_mut(start, len)
        .ok_or_else(crate::sys::pal::unsupported_err)?;
    buf.get_mut(..len)
        .ok_or_else(crate::sys::pal::unsupported_err)?
        .copy_from_slice(window);
    Ok(len)
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_named_stream(&STDOUT_PIPE, buf)
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
        write_named_stream(&STDERR_PIPE, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Write to a Command-redirected pipe when present, otherwise the terminal
/// or serial log.
fn write_named_stream(pipe: &Mutex<Option<Pipe>>, buf: &[u8]) -> io::Result<usize> {
    if let Some(pipe) = pipe.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return pipe.write(buf);
    }
    write_stream(buf)
}

/// Write to the controlling terminal when this process has one, falling back
/// to the serial log otherwise. `Log` is a whole-line syscall with no fd
/// distinction (`kernel/src/syscall/mod.rs`), so a terminal-less process's
/// stdout and stderr land on the same serial sink.
fn write_stream(buf: &[u8]) -> io::Result<usize> {
    match with_terminal(|channel| terminal_write(channel, buf)) {
        Some(result) => result,
        None => write_log(buf),
    }
}

/// One `OP_SEND` round trip: stage the bytes in the channel's shared data
/// window, then describe them to the provider.
fn terminal_write(channel: &mut SyncChannel, buf: &[u8]) -> io::Result<usize> {
    let len = buf.len().min(ask_io::terminal::DATA_LEN as usize);
    let staged = buf
        .get(..len)
        .ok_or_else(crate::sys::pal::unsupported_err)?;
    let window = channel
        .shared_region_mut(ask_io::terminal::DATA_OFFSET as usize, len)
        .ok_or_else(crate::sys::pal::unsupported_err)?;
    window.copy_from_slice(staged);

    let descriptor = ask_io::terminal::TerminalBuffer::new(0, len as u32)
        .ok_or_else(crate::sys::pal::unsupported_err)?;
    let mut request = [0u8; 8];
    let payload = ask_io::terminal::encode_terminal_send_request(&mut request, descriptor);
    let completion = channel.call(ask_io::terminal::OP_SEND, payload)?;
    if completion.result < 0 {
        return Err(io::const_error!(
            io::ErrorKind::Other,
            "stdout: terminal write failed"
        ));
    }
    Ok(len)
}

fn write_log(buf: &[u8]) -> io::Result<usize> {
    match core::str::from_utf8(buf) {
        Ok(s) => {
            ask_sys::log(s);
            Ok(buf.len())
        }
        Err(e) => {
            let valid = e.valid_up_to();
            if valid == 0 {
                return Err(io::const_error!(io::ErrorKind::InvalidData, "stdio: invalid UTF-8"));
            }
            // Safety: `valid` bytes were just reported valid by `from_utf8`.
            ask_sys::log(unsafe { core::str::from_utf8_unchecked(&buf[..valid]) });
            Ok(valid)
        }
    }
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}

/// `std` uses this to decide whether a stdout/stderr failure was "no such
/// descriptor" and therefore safe to ignore while printing a panic. Neither
/// the terminal path nor `Log` reports a missing descriptor — a terminal-less
/// process falls back to `Log` instead of failing — so no error here is one.
pub fn is_ebadf(_err: &io::Error) -> bool {
    false
}
