//! ASK-specific definitions.

#![stable(feature = "rust1", since = "1.0.0")]

pub mod ffi;
pub mod fs;

/// Drain buffered standard output without driving an executor.
#[doc(hidden)]
#[unstable(feature = "ask_runtime", issue = "none")]
pub async fn drain_stdio() -> crate::io::Result<()> {
    crate::io::drain_ask().await
}

/// A prelude for conveniently writing platform-specific code.
///
/// Includes all extension traits, and some important type definitions.
#[stable(feature = "rust1", since = "1.0.0")]
pub mod prelude {
    #[doc(no_inline)]
    #[stable(feature = "rust1", since = "1.0.0")]
    pub use super::ffi::{OsStrExt, OsStringExt};
    #[doc(no_inline)]
    #[stable(feature = "rust1", since = "1.0.0")]
    pub use super::fs::{FileTypeExt, MetadataExt};
}
