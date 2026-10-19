// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Filesystem helpers: type detection, content hashing and free-space reporting.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Chunk size for streaming file reads (keeps peak memory constant regardless
/// of file size).
const HASH_CHUNK: usize = 64 * 1024;

/// Stream a file through SHA-256, returning `(hex digest, size in bytes)`.
///
/// Unlike a whole-file `fs::read` + hash, this uses constant memory and works
/// on files much larger than RAM.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Detect the filesystem name for the volume containing `path`.
///
/// Falls back to `"unknown"` rather than failing so callers can always choose a
/// safe copy-based strategy.
pub fn detect_fs_name(path: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = nix::sys::statfs::statfs(path) {
            return magic_to_name(stat.filesystem_type().0);
        }
        "unknown".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        "apfs".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        "unknown".to_string()
    }
}

/// Map a Linux `statfs.f_type` magic to a human-readable name.
#[cfg(target_os = "linux")]
fn magic_to_name(magic: i64) -> String {
    // Magic constants (from `linux/magic.h`). Some only exist on certain libc
    // versions, so compare numerically.
    const EXT2: i64 = 0xEF53;
    const BTRFS: i64 = 0x9123_683E;
    const XFS: i64 = 0x5846_5342;
    const ZFS: i64 = 0x2FC1_2FC1;
    const TMPFS: i64 = 0x0102_1994;
    const NFS: i64 = 0x6969;
    const MSDOS: i64 = 0x4d44;

    match magic {
        EXT2 => "ext4".to_string(),
        BTRFS => "btrfs".to_string(),
        XFS => "xfs".to_string(),
        ZFS => "zfs".to_string(),
        TMPFS => "tmpfs".to_string(),
        NFS => "nfs".to_string(),
        MSDOS => "vfat".to_string(),
        _ => format!("unknown(0x{magic:x})"),
    }
}

/// Report free bytes available on the filesystem containing `path`.
pub fn available_bytes(path: &Path) -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = nix::sys::statfs::statfs(path)?;
        Ok(stat.blocks_available() as u64 * stat.block_size() as u64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_name() {
        let name = detect_fs_name(Path::new("."));
        assert!(!name.is_empty());
    }
}
