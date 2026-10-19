// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Low-level copy-on-write reflink primitives and filesystem probing.
//!
//! On Linux we use the `FICLONE` ioctl, which is supported by CoW filesystems
//! such as Btrfs and XFS. When unsupported (Ext4/NTFS/FAT32/…) the caller falls
//! back to a byte copy so the engine never panics.

use anyhow::{Context, Result};
use std::path::Path;

#[cfg(target_os = "linux")]
use nix::ioctl_write_int;

// `FICLONE` (`_IOW(0x94, 9, int)`): the ioctl *argument* is the source fd
// passed **by value** (`ioctl(dst_fd, FICLONE, src_fd)`), not a pointer —
// `ioctl_write_int!` is the matching nix variant. (`ioctl_readwrite!` would
// pass `&mut fd`, which the kernel misinterprets as an fd number and rejects
// with EBADF, silently degrading every clone to a full copy.)
#[cfg(target_os = "linux")]
ioctl_write_int!(ficlone, 0x94, 9);

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
    // SAFETY: `dest` is a valid open fd; the source fd is passed by value as
    // the ioctl argument, exactly as `FICLONE` expects.
    unsafe { ficlone(dest.as_raw_fd(), source.as_raw_fd() as libc::c_ulong) }
        .map_err(anyhow::Error::from)
        .with_context(|| format!("FICLONE failed for {}", src.display()))?;
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

    /// Exercise the real `FICLONE` path. On CoW filesystems (btrfs/xfs) this
    /// must succeed; elsewhere it may only fail with the classic
    /// "unsupported operation" errnos. Any other errno (notably `EBADF`/`EINVAL`)
    /// indicates a broken ioctl call shape rather than an unsupporting
    /// filesystem — that regression silently degraded every workspace to
    /// full copies, so it must fail the test.
    #[cfg(target_os = "linux")]
    #[test]
    fn reflink_uses_a_correct_ficlone_call() {
        use nix::errno::Errno;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, vec![7u8; 1 << 20]).unwrap();

        match reflink(&src, &dst) {
            Ok(()) => {
                assert_eq!(std::fs::read(&dst).unwrap(), vec![7u8; 1 << 20]);
                assert_eq!(std::fs::metadata(&src).unwrap().len(), 1 << 20);
            }
            Err(e) => {
                let errno = e
                    .chain()
                    .find_map(|c| c.downcast_ref::<Errno>())
                    .copied();
                assert!(
                    matches!(
                        errno,
                        Some(Errno::EOPNOTSUPP) | Some(Errno::ENOTTY) | Some(Errno::EXDEV)
                            | Some(Errno::ENOSYS)
                    ),
                    "reflink failed with an errno that suggests a broken ioctl \
                     (expected only 'filesystem does not support reflink'): {errno:?}: {e:#}"
                );
            }
        }
    }
}
