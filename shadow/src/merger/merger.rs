// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Staging and atomic application of workspace changes into the project root.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::co_w::cow::{RESERVED_DIRS, RESERVED_FILES};

fn is_reserved(rel: &Path) -> bool {
    rel.components().any(|c| {
        RESERVED_DIRS.iter().any(|r| c.as_os_str() == *r)
            || RESERVED_FILES.iter().any(|r| c.as_os_str() == *r)
    })
}

/// Copy the tracked contents of `src` into `staging`, skipping engine-internal
/// files and directories.
pub fn stage_changes(src: &Path, staging: &Path) -> Result<()> {
    fs::create_dir_all(staging)?;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        let rel = match path.strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => continue,
        };
        if is_reserved(&rel) {
            continue;
        }
        let target = staging.join(&rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        }
    }
    Ok(())
}

/// Atomically swap two directories using `renameat2(RENAME_EXCHANGE)` when
/// available, falling back to a two-rename sequence otherwise.
///
/// Both paths must exist and reside on the same filesystem.
pub fn swap_dirs(a: &Path, b: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let a_c = std::ffi::CString::new(a.as_os_str().as_encoded_bytes()).ok();
        let b_c = std::ffi::CString::new(b.as_os_str().as_encoded_bytes()).ok();
        if let (Some(a_c), Some(b_c)) = (a_c, b_c) {
            // SAFETY: both pointers are valid NUL-terminated C strings.
            let rc = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    a_c.as_ptr(),
                    libc::AT_FDCWD,
                    b_c.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            // Fall through to the non-atomic fallback below.
        }
    }

    // Fallback: two renames. Not a single atomic op, but each rename is atomic,
    // so corruption can only occur if the process is killed between them.
    let tmp = swap_temp_path(a);
    let _ = fs::remove_dir_all(&tmp);
    fs::rename(a, &tmp).context("swap: move a aside")?;
    fs::rename(b, a).context("swap: move b into a")?;
    fs::rename(&tmp, b).context("swap: move temp into b")?;
    Ok(())
}

/// Replace `target` with `replacement` atomically when possible.
///
/// `replacement` must live on the same filesystem as `target`.
pub fn replace_path(replacement: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if target.exists() {
        // Renaming over a file is atomic. Renaming over a non-empty directory is
        // not permitted, so fall back to a remove-then-rename.
        if target.is_dir() {
            fs::remove_dir_all(target)?;
        }
    }
    fs::rename(replacement, target).with_context(|| {
        format!(
            "moving {} into place at {}",
            replacement.display(),
            target.display()
        )
    })
}

/// Apply every file from `staging` into `root` using per-file atomic renames.
///
/// Returns the number of files applied.
pub fn apply_atomic(staging: &Path, root: &Path) -> Result<usize> {
    let mut applied = 0usize;
    for entry in WalkDir::new(staging).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(staging)?;
        if is_reserved(rel) {
            continue;
        }
        let target = root.join(rel);
        replace_path(entry.path(), &target)?;
        applied += 1;
    }
    Ok(applied)
}

/// Apply a specific set of relative paths from `staging` into `root` using
/// per-file atomic renames. Paths absent from `staging` are ignored.
pub fn apply_paths(staging: &Path, root: &Path, paths: &[String]) -> Result<usize> {
    let mut applied = 0usize;
    for rel in paths {
        let source = staging.join(rel);
        if !source.exists() {
            continue;
        }
        let target = root.join(rel);
        replace_path(&source, &target)?;
        applied += 1;
    }
    Ok(applied)
}

fn swap_temp_path(p: &Path) -> PathBuf {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut tmp = p.to_path_buf();
    tmp.set_file_name(format!(".{name}.shadow-swap"));
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_skips_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join(".shadow")).unwrap();
        fs::write(src.join(".shadow/x"), b"secret").unwrap();
        fs::write(src.join(".shadow-lock"), b"lock").unwrap();
        fs::write(src.join("keep.txt"), b"keep").unwrap();

        let staging = dir.path().join("staging");
        stage_changes(&src, &staging).unwrap();
        assert!(staging.join("keep.txt").exists());
        assert!(!staging.join(".shadow").exists());
        assert!(!staging.join(".shadow-lock").exists());
    }

    #[test]
    fn apply_atomic_moves_files() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let root = dir.path().join("root");
        fs::create_dir_all(staging.join("nested")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(staging.join("nested/a.txt"), b"a").unwrap();
        fs::write(staging.join("b.txt"), b"b").unwrap();

        let n = apply_atomic(&staging, &root).unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs::read(root.join("nested/a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"b");
        // Files were moved, not copied.
        assert!(!staging.join("b.txt").exists());
    }

    #[test]
    fn replace_path_overwrites_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t.txt");
        let repl = dir.path().join("r.txt");
        fs::write(&target, b"old").unwrap();
        fs::write(&repl, b"new").unwrap();
        replace_path(&repl, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }
}
