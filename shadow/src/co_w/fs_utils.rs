// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Low-level copy-on-write reflink primitives and filesystem probing.
//!
//! On Linux we use the `FICLONE` ioctl, which is supported by CoW filesystems
//! such as Btrfs and XFS. When unsupported (Ext4/NTFS/FAT32/…) the caller falls
//! back to a byte copy so the engine never panics.

use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "linux")]
use nix::ioctl_readwrite;

// `FICLONE` (`_IOW(0x94, 9, int)`) — clone the file referenced by the source
// fd onto the destination fd.
#[cfg(target_os = "linux")]
ioctl_readwrite!(ficlone, 0x94, 9, libc::c_int);

/// Strategy used when materialising a workspace from its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowStrategy {
    /// Native reflink (zero-copy, instant).
    Reflink,
    /// Full byte copy fallback for non-CoW filesystems.
    Copy,
}

impl CowStrategy {
    /// Choose a strategy for the given filesystem name.
    pub fn for_fs(fs: &str) -> Self {
        if is_cow_supported_fs(fs) {
            CowStrategy::Reflink
        } else {
            CowStrategy::Copy
        }
    }
}

/// Filesystems known to support native reflinks.
pub fn is_cow_supported_fs(fs: &str) -> bool {
    matches!(
        fs.to_ascii_lowercase().as_str(),
        "apfs" | "btrfs" | "xfs" | "zfs" | "bcachefs"
    )
}

/// Filesystems that definitely lack native reflink support.
pub fn is_problematic_fs(fs: &str) -> bool {
    matches!(
        fs.to_ascii_lowercase().as_str(),
        "ext4" | "ext3" | "ext2" | "ntfs" | "fat" | "fat32" | "vfat" | "exfat" | "tmpfs"
    )
}

/// Attempt a native reflink of `src` onto `dst`.
///
/// Returns an error (rather than panicking) when the operation is unsupported
/// so callers can transparently fall back to [`copy_file`].
#[cfg(target_os = "linux")]
pub fn reflink(src: &Path, dst: &Path) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let source = OpenOptions::new().read(true).open(src)?;
    let dest = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)?;
    let mut src_fd = source.as_raw_fd();
    // SAFETY: `dest` is a valid open fd and `src_fd` points to a valid int.
    unsafe { ficlone(dest.as_raw_fd(), &mut src_fd) }
        .map_err(|e| anyhow::anyhow!("FICLONE failed for {}: {e}", src.display()))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn reflink(_src: &Path, _dst: &Path) -> Result<()> {
    Err(anyhow::anyhow!(
        "native reflinks are only implemented on Linux"
    ))
}

/// Copy `src` onto `dst`, preserving the parent directory.
pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_filesystems() {
        assert!(is_cow_supported_fs("btrfs"));
        assert!(is_cow_supported_fs("APFS"));
        assert!(!is_cow_supported_fs("ext4"));
        assert!(is_problematic_fs("ext4"));
        assert!(is_problematic_fs("NTFS"));
        assert_eq!(CowStrategy::for_fs("btrfs"), CowStrategy::Reflink);
        assert_eq!(CowStrategy::for_fs("ext4"), CowStrategy::Copy);
    }

    #[test]
    fn copy_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("nested/b.txt");
        std::fs::write(&src, b"hello shadow").unwrap();
        copy_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello shadow");
    }
}
