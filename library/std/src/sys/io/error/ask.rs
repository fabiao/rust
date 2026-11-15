//! Raw OS errors on ask are `askrust` runtime error codes.

use crate::{fmt, io};

pub fn errno() -> io::RawOsError {
    0
}

pub fn is_interrupted(code: io::RawOsError) -> bool {
    code == i32::from(askrust::error::code::INTERRUPTED)
}

pub fn decode_error_kind(code: io::RawOsError) -> io::ErrorKind {
    use askrust::error::code;
    let Ok(code) = u16::try_from(code) else {
        return io::ErrorKind::Uncategorized;
    };
    match code {
        code::NOT_FOUND => io::ErrorKind::NotFound,
        code::PERMISSION_DENIED => io::ErrorKind::PermissionDenied,
        code::CONNECTION_REFUSED => io::ErrorKind::ConnectionRefused,
        code::CONNECTION_RESET => io::ErrorKind::ConnectionReset,
        code::CONNECTION_ABORTED => io::ErrorKind::ConnectionAborted,
        code::NOT_CONNECTED => io::ErrorKind::NotConnected,
        code::ADDR_IN_USE => io::ErrorKind::AddrInUse,
        code::ADDR_NOT_AVAILABLE => io::ErrorKind::AddrNotAvailable,
        code::BROKEN_PIPE => io::ErrorKind::BrokenPipe,
        code::ALREADY_EXISTS => io::ErrorKind::AlreadyExists,
        code::WOULD_BLOCK => io::ErrorKind::WouldBlock,
        code::INVALID_INPUT => io::ErrorKind::InvalidInput,
        code::INVALID_DATA => io::ErrorKind::InvalidData,
        code::TIMED_OUT => io::ErrorKind::TimedOut,
        code::WRITE_ZERO => io::ErrorKind::WriteZero,
        code::INTERRUPTED => io::ErrorKind::Interrupted,
        code::UNSUPPORTED => io::ErrorKind::Unsupported,
        code::UNEXPECTED_EOF => io::ErrorKind::UnexpectedEof,
        code::OUT_OF_MEMORY => io::ErrorKind::OutOfMemory,
        code::QUOTA_EXCEEDED => io::ErrorKind::QuotaExceeded,
        code::RESOURCE_BUSY => io::ErrorKind::ResourceBusy,
        code::NOT_A_DIRECTORY => io::ErrorKind::NotADirectory,
        code::IS_A_DIRECTORY => io::ErrorKind::IsADirectory,
        code::DIRECTORY_NOT_EMPTY => io::ErrorKind::DirectoryNotEmpty,
        code::READ_ONLY_FILESYSTEM => io::ErrorKind::ReadOnlyFilesystem,
        code::CROSSES_DEVICES => io::ErrorKind::CrossesDevices,
        code::INVALID_FILENAME => io::ErrorKind::InvalidFilename,
        code::STORAGE_FULL => io::ErrorKind::StorageFull,
        code::STALE_NETWORK_FILE_HANDLE => io::ErrorKind::StaleNetworkFileHandle,
        code::OTHER => io::ErrorKind::Other,
        _ => io::ErrorKind::Uncategorized,
    }
}

pub fn format_error(code: io::RawOsError, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match u16::try_from(code) {
        Ok(code) => f.write_str(askrust::Error::from_code(code).description()),
        Err(_) => f.write_str("unknown ASK runtime error"),
    }
}
