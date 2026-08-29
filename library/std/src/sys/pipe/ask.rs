//! Same-process anonymous pipes over `askio::pipe` and `askipc` self-pairs.
//!
//! Donor: askposix `pipe2` (adapt the self-pair + `SyncPipeReader` shape;
//! rewrite the std `Pipe` type around it). Motor OS's PAL `pipe` is
//! `UNSUPPORTED_PLATFORM`.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::sync::{Arc, Mutex};
use crate::{fmt, mem};

enum Inner {
    Writer {
        channel: ask_ipc::sync::SyncChannel,
        eof_sent: bool,
    },
    Reader {
        reader: ask_ipc::sync::SyncPipeReader,
        eof_seen: bool,
    },
}

pub struct Pipe {
    inner: Arc<Mutex<Inner>>,
}

fn map_ipc_error(error: ask_ipc::sync::Error) -> io::Error {
    match error {
        ask_ipc::sync::Error::PeerClosed => {
            io::const_error!(io::ErrorKind::BrokenPipe, "pipe peer closed")
        }
        _ => io::const_error!(io::ErrorKind::Other, "pipe transport failed"),
    }
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let (writer, acceptor_virt) =
        ask_ipc::sync::SyncChannel::connect_self_pair(ask_io::pipe::CHANNEL_PAGES)
            .map_err(map_ipc_error)?;
    // Safety: `acceptor_virt` is the untouched acceptor mapping returned by
    // this `connect_self_pair` call, which already installed both views.
    let reader = unsafe {
        ask_ipc::sync::SyncPipeReader::attach_acceptor(acceptor_virt, ask_io::pipe::CHANNEL_PAGES)
    };
    Ok((
        Pipe {
            inner: Arc::new(Mutex::new(Inner::Reader { reader, eof_seen: false })),
        },
        Pipe {
            inner: Arc::new(Mutex::new(Inner::Writer { channel: writer, eof_sent: false })),
        },
    ))
}

pub(crate) fn writer_to_peer(peer: u32) -> io::Result<Pipe> {
    let channel = ask_ipc::sync::SyncChannel::connect(peer, ask_io::pipe::CHANNEL_PAGES)
        .map_err(map_ipc_error)?;
    Ok(Pipe {
        inner: Arc::new(Mutex::new(Inner::Writer { channel, eof_sent: false })),
    })
}

pub(crate) fn accept_reader() -> io::Result<Pipe> {
    let (virt, _peer, pages, _) = ask_sys::channel_accept().map_err(crate::sys::map_ask_error)?;
    // Safety: `channel_accept` installed this acceptor mapping; the peer
    // creator already initialized the rings.
    let reader = unsafe { ask_ipc::sync::SyncPipeReader::attach_acceptor(virt, pages) };
    Ok(Pipe {
        inner: Arc::new(Mutex::new(Inner::Reader { reader, eof_seen: false })),
    })
}

impl Pipe {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Pipe { inner: Arc::clone(&self.inner) })
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Inner::Reader { reader, eof_seen } = &mut *guard else {
                return Err(io::const_error!(
                    io::ErrorKind::BrokenPipe,
                    "read on pipe write end"
                ));
            };
            if *eof_seen {
                return Ok(0);
            }
            let submission = match reader.recv_submission_raw() {
                Ok(submission) => submission,
                Err(ask_ipc::sync::Error::PeerClosed) => {
                    *eof_seen = true;
                    return Ok(0);
                }
                Err(error) => return Err(map_ipc_error(error)),
            };
            match submission.opcode {
                ask_io::pipe::OP_CLOSE => {
                    let _ = reader.complete(submission.user_data, 0, &[]);
                    *eof_seen = true;
                    return Ok(0);
                }
                ask_io::pipe::OP_CANCEL | ask_io::pipe::OP_RESIZE => {
                    let _ = reader.complete(submission.user_data, 0, &[]);
                    continue;
                }
                ask_io::pipe::OP_SEND => {
                    let Some(buffer) =
                        ask_io::pipe::decode_pipe_send_request(submission.payload())
                    else {
                        return Err(io::const_error!(
                            io::ErrorKind::InvalidData,
                            "pipe send descriptor"
                        ));
                    };
                    let n = (buffer.len as usize).min(buf.len());
                    let Some(window) = reader.shared_region_mut(
                        ask_io::pipe::DATA_OFFSET as usize + buffer.offset as usize,
                        n,
                    ) else {
                        return Err(io::const_error!(
                            io::ErrorKind::InvalidData,
                            "pipe send window"
                        ));
                    };
                    buf.get_mut(..n)
                        .ok_or_else(crate::sys::pal::unsupported_err)?
                        .copy_from_slice(window);
                    let _ = reader.complete(submission.user_data, n as i32, &[]);
                    return Ok(n);
                }
                _ => {
                    return Err(io::const_error!(
                        io::ErrorKind::InvalidData,
                        "unexpected pipe opcode"
                    ));
                }
            }
        }
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|buf| self.read(buf), cursor)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|buf| self.read(buf), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut scratch = [0u8; 4096];
        let start = buf.len();
        loop {
            let n = self.read(&mut scratch)?;
            if n == 0 {
                return Ok(buf.len() - start);
            }
            buf.extend_from_slice(scratch.get(..n).unwrap_or(&[]));
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Inner::Writer { channel, eof_sent } = &mut *guard else {
            return Err(io::const_error!(
                io::ErrorKind::BrokenPipe,
                "write on pipe read end"
            ));
        };
        if *eof_sent {
            return Err(io::const_error!(io::ErrorKind::BrokenPipe, "pipe write after close"));
        }
        let n = buf.len().min(ask_io::pipe::DATA_LEN as usize);
        let staged = buf.get(..n).ok_or_else(crate::sys::pal::unsupported_err)?;
        let window = channel
            .shared_region_mut(ask_io::pipe::DATA_OFFSET as usize, n)
            .ok_or_else(crate::sys::pal::unsupported_err)?;
        window.copy_from_slice(staged);
        let buffer = ask_io::pipe::PipeBuffer::new(0, n as u32)
            .ok_or_else(crate::sys::pal::unsupported_err)?;
        let mut request = [0u8; 8];
        let request = ask_io::pipe::encode_pipe_send_request(&mut request, buffer);
        channel.submit(ask_io::pipe::OP_SEND, request).map_err(map_ipc_error)?;
        let completion = channel.recv_completion_raw().map_err(map_ipc_error)?;
        Ok(completion.result.max(0) as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|buf| self.write(buf), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    fn send_close(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Inner::Writer { channel, eof_sent } = &mut *guard else {
            return;
        };
        if mem::replace(eof_sent, true) {
            return;
        }
        if channel.submit(ask_io::pipe::OP_CLOSE, &[]).is_ok() {
            let _ = channel.recv_completion_raw();
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.send_close();
        }
    }
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe").finish_non_exhaustive()
    }
}
