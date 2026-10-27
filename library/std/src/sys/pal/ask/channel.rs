//! std error and blocking-wait adaptation over the shared ASK transport.

use crate::io;

pub use ask_ipc::sync::Completion;

/// Bounded cadence for PAL readiness and reconciliation probes.
pub(crate) const POLL_INTERVAL_MS: u64 = 25;

pub struct SyncChannel(ask_ipc::SyncChannel);

fn map_error(error: ask_ipc::sync::Error) -> io::Error {
    use ask_ipc::sync::Error;
    match error {
        Error::Abi(error) => super::map_ask_error(error),
        Error::PeerClosed => io::const_error!(
            io::ErrorKind::ConnectionAborted,
            "ASK channel peer closed before a matching reply",
        ),
        Error::Full => io::const_error!(io::ErrorKind::WouldBlock, "ASK channel capacity exhausted"),
        Error::PayloadTooLarge => io::const_error!(
            io::ErrorKind::InvalidInput,
            "ASK channel request exceeds its payload window",
        ),
        Error::Malformed | Error::UnexpectedCompletion => io::const_error!(
            io::ErrorKind::InvalidData,
            "ASK channel reply is malformed or uncorrelated",
        ),
    }
}

impl SyncChannel {
    pub fn create_leased(token: u32, pages: u64) -> io::Result<Self> {
        ask_ipc::SyncChannel::create_leased(token, pages).map(Self).map_err(map_error)
    }

    pub fn virt(&self) -> u64 {
        self.0.virt()
    }

    pub fn shared_region_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        self.0.shared_region_mut(offset, len)
    }

    pub fn submit(&mut self, opcode: u32, payload: &[u8]) -> io::Result<u64> {
        self.0.submit(opcode, payload, None).map_err(map_error)
    }

    fn receive(&mut self) -> io::Result<Completion> {
        #[cfg(ask_class1_verify)]
        return Err(io::const_error!(
            io::ErrorKind::Unsupported,
            "Class 1 verification tripwire: blocking std ASK channel wait is prohibited",
        ));
        self.0.recv_completion_raw().map_err(map_error)
    }

    fn wait_for_completion(&mut self, identity: u64) -> io::Result<Completion> {
        loop {
            let completion = self.receive()?;
            if completion.user_data == identity {
                return Ok(completion);
            }
        }
    }

    pub fn call(&mut self, opcode: u32, payload: &[u8]) -> io::Result<Completion> {
        let identity = loop {
            match self.submit(opcode, payload) {
                Ok(identity) => break identity,
                // Earlier nonblocking aborts still own their reply reservations.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let _ = self.receive()?;
                }
                Err(error) => return Err(error),
            }
        };
        self.wait_for_completion(identity)
    }
}
