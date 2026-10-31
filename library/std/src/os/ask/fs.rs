//! ASK-specific extensions to primitives in the [`std::fs`] module.
//!
//! [`std::fs`]: crate::fs

#![stable(feature = "rust1", since = "1.0.0")]

use crate::path::Path;
use crate::sys::AsInner;
use crate::{fs, io};

/// ASK-specific extensions to [`fs::Metadata`].
///
/// A node's identity is the triple of [`mount_id`], [`node_id`] and
/// [`provider_generation`]; two metadata values name the same node only when
/// all three are equal.
///
/// [`mount_id`]: MetadataExt::mount_id
/// [`node_id`]: MetadataExt::node_id
/// [`provider_generation`]: MetadataExt::provider_generation
#[stable(feature = "rust1", since = "1.0.0")]
pub trait MetadataExt {
    /// Returns the identity of the mounted filesystem holding the node.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn mount_id(&self) -> u64;
    /// Returns the node's identity within its mount.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn node_id(&self) -> u64;
    /// Returns the generation of the provider process serving the mount.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn provider_generation(&self) -> u64;
    /// Returns the number of hard links to the node.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn links(&self) -> u32;
    /// Returns the node's permission bits.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn mode(&self) -> u32;
    /// Returns the node's owner.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn owner(&self) -> u64;
    /// Returns the node's group.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn group(&self) -> u64;
    /// Returns the bytes of storage allocated to the node.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn allocated_size(&self) -> u64;
    /// Returns the last status change time, in nanoseconds from the Unix epoch.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn changed_ns(&self) -> i64;
}

#[stable(feature = "rust1", since = "1.0.0")]
impl MetadataExt for fs::Metadata {
    fn mount_id(&self) -> u64 {
        self.as_inner().native().mount_id
    }
    fn node_id(&self) -> u64 {
        self.as_inner().native().node_id
    }
    fn provider_generation(&self) -> u64 {
        self.as_inner().native().provider_generation
    }
    fn links(&self) -> u32 {
        self.as_inner().native().links
    }
    fn mode(&self) -> u32 {
        self.as_inner().native().mode
    }
    fn owner(&self) -> u64 {
        self.as_inner().native().owner
    }
    fn group(&self) -> u64 {
        self.as_inner().native().group
    }
    fn allocated_size(&self) -> u64 {
        self.as_inner().native().allocated_size
    }
    fn changed_ns(&self) -> i64 {
        self.as_inner().native().ctime_ns
    }
}

/// Creates a new symbolic link at `link` pointing to `original`.
///
/// A relative `original` is stored as given. An absolute `original` must be
/// served by the same provider as `link`.
#[stable(feature = "rust1", since = "1.0.0")]
pub fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    crate::sys::fs::symlink(original.as_ref(), link.as_ref())
}
