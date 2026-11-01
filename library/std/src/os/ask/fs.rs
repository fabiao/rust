//! ASK-specific extensions to primitives in the [`std::fs`] module.
//!
//! [`std::fs`]: crate::fs

#![stable(feature = "rust1", since = "1.0.0")]

use crate::ffi::OsString;
use crate::path::Path;
use crate::sys::{AsInner, FromInner};
use crate::{fs, io, sys};

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

/// ASK-specific extensions to [`fs::FileType`] for the node kinds a provider
/// reports beyond files, directories and symbolic links.
#[stable(feature = "rust1", since = "1.0.0")]
pub trait FileTypeExt {
    /// Returns `true` if this file type is a block device.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn is_block_device(&self) -> bool;
    /// Returns `true` if this file type is a character device.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn is_char_device(&self) -> bool;
    /// Returns `true` if this file type is a FIFO.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn is_fifo(&self) -> bool;
    /// Returns `true` if this file type is a socket.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn is_socket(&self) -> bool;
}

#[stable(feature = "rust1", since = "1.0.0")]
impl FileTypeExt for fs::FileType {
    fn is_block_device(&self) -> bool {
        self.as_inner().kind() == ask_io::fs::NodeKind::Block as u8
    }
    fn is_char_device(&self) -> bool {
        self.as_inner().kind() == ask_io::fs::NodeKind::Character as u8
    }
    fn is_fifo(&self) -> bool {
        self.as_inner().kind() == ask_io::fs::NodeKind::Fifo as u8
    }
    fn is_socket(&self) -> bool {
        self.as_inner().kind() == ask_io::fs::NodeKind::Socket as u8
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

/// ASK-specific extensions to [`fs::Permissions`], carrying the full mode as
/// on Unix.
#[stable(feature = "rust1", since = "1.0.0")]
pub trait PermissionsExt {
    /// Returns the permission, set-id and sticky bits.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn mode(&self) -> u32;
    /// Replaces the mode bits.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn set_mode(&mut self, mode: u32);
    /// Creates permissions from mode bits.
    #[stable(feature = "rust1", since = "1.0.0")]
    fn from_mode(mode: u32) -> Self;
}

#[stable(feature = "rust1", since = "1.0.0")]
impl PermissionsExt for fs::Permissions {
    fn mode(&self) -> u32 {
        self.as_inner().mode()
    }
    fn set_mode(&mut self, mode: u32) {
        *self = fs::Permissions::from_inner(sys::fs::FilePermissions::from_mode(mode));
    }
    fn from_mode(mode: u32) -> Self {
        fs::Permissions::from_inner(sys::fs::FilePermissions::from_mode(mode))
    }
}

/// ASK-specific operations on an open [`fs::Dir`], resolved beneath that
/// directory by its provider rather than through a path from the root.
#[unstable(feature = "dirfd", issue = "120426")]
pub trait DirExt {
    /// Returns the names directly beneath this directory.
    #[unstable(feature = "dirfd", issue = "120426")]
    fn entry_names(&self) -> io::Result<crate::vec::Vec<OsString>>;
    /// Replaces the permissions of `path` beneath this directory, following a
    /// final symlink.
    #[unstable(feature = "dirfd", issue = "120426")]
    fn set_permissions_at<P: AsRef<Path>>(&self, path: P, perm: fs::Permissions) -> io::Result<()>;
}

#[unstable(feature = "dirfd", issue = "120426")]
impl DirExt for fs::Dir {
    fn entry_names(&self) -> io::Result<crate::vec::Vec<OsString>> {
        self.as_inner().entry_names()
    }
    fn set_permissions_at<P: AsRef<Path>>(&self, path: P, perm: fs::Permissions) -> io::Result<()> {
        self.as_inner().set_permissions_at(path.as_ref(), perm.as_inner().clone())
    }
}
