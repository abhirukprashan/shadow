// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Recursive workspace materialisation built on top of the reflink primitives.

use crate::co_w::fs_utils::{self, CowStrategy};
use anyhow::Result;
use std::path::Path;
use walkdir::WalkDir;

/// Internal directories that must never be copied into a workspace. This
/// includes engine state (which would recurse) and per-workspace build
/// artifacts (`target/`, `node_modules/`, `.shadow-artifacts/`) which are
/// isolated per workspace rather than shared.
pub const RESERVED_DIRS: &[&str] = &[
    ".shadow",
    ".shadow-workspaces",
    ".shadow-staging",
    ".shadow-artifacts",
    ".shadow-baseline",
    ".git",
    "target",
    "node_modules",
];

/// Engine-internal files that live alongside workspace contents and are never
/// treated as tracked content.
pub const RESERVED_FILES: &[&str] = &[".shadow-lock", ".shadow-baseline.json"];

/// Whether any component of `rel` names an engine-internal directory or file.
///
/// Single source of truth for every walk in the engine; allocation-free
/// (compares `OsStr` components directly).
pub fn is_reserved(rel: &Path) -> bool {
    rel.components().any(|c| {
        RESERVED_DIRS.iter().any(|r| c.as_os_str() == *r)
            || RESERVED_FILES.iter().any(|r| c.as_os_str() == *r)
    })
}

/// Materialise `src` into `dst` using CoW reflinks where available, falling
/// back to byte copies. Engine-internal directories are skipped.
///
/// Returns the number of files materialised and the strategy ultimately used.
pub fn materialize(src: &Path, dst: &Path, strategy: CowStrategy) -> Result<(usize, CowStrategy)> {
    materialize_with(src, dst, strategy, |_, _| Ok(()))
}

/// Like [`materialize`], but invokes `on_file(src_path, rel)` for every copied
/// file while the tree is being walked.
///
/// This lets callers (e.g. `Engine::create`) compute a content manifest in the
/// *same* pass as the copy, instead of paying for an extra full tree traversal.
pub fn materialize_with<F>(
    src: &Path,
    dst: &Path,
    strategy: CowStrategy,
    mut on_file: F,
) -> Result<(usize, CowStrategy)>
where
    F: FnMut(&Path, &str) -> Result<()>,
{
    std::fs::create_dir_all(dst)?;
    let mut files = 0usize;
    let mut used = strategy;

    // WalkDir yields every directory before its children, so parent targets
    // always exist by the time a file inside them is copied — no per-file
    // `create_dir_all` needed.
    for entry in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Always allow the root itself; prune reserved subtrees.
            if e.depth() == 0 {
                return true;
            }
            !is_reserved(e.path().strip_prefix(src).unwrap_or(e.path()))
        })
    {
        let entry = entry?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        let target = dst.join(rel);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            // Prefer the requested strategy, but degrade per-file if needed.
            if used == CowStrategy::Reflink {
                match fs_utils::reflink(entry.path(), &target) {
                    Ok(()) => {}
                    Err(_) => {
                        fs_utils::copy_file(entry.path(), &target)?;
                        used = CowStrategy::Copy;
                    }
                }
            } else {
                fs_utils::copy_file(entry.path(), &target)?;
            }
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            on_file(entry.path(), &rel_str)?;
            files += 1;
        }
    }

    Ok((files, used))
}

/// Aggregate size of the tree [`materialize`] would copy.
///
/// Uses the exact same reserved-directory pruning as the copy walk, so the
/// numbers describe the real clone cost: engine state, `.git`, `target/`,
/// `node_modules/` etc. never count because they are never copied. Returns
/// `(bytes, files)`.
pub fn tree_stats(src: &Path) -> Result<(u64, u64)> {
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            !is_reserved(e.path().strip_prefix(src).unwrap_or(e.path()))
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            files += 1;
            bytes += entry.metadata()?.len();
        }
    }
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_copies_tree() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("src/deep")).unwrap();
        std::fs::write(base.path().join("src/root.txt"), b"root").unwrap();
        std::fs::write(base.path().join("src/deep/leaf.txt"), b"leaf").unwrap();

        let out = base.path().join("out");
        let (count, _) = materialize(&base.path().join("src"), &out, CowStrategy::Copy).unwrap();
        assert_eq!(count, 2);
        assert_eq!(std::fs::read(out.join("deep/leaf.txt")).unwrap(), b"leaf");
    }

    #[test]
    fn materialize_skips_reserved_dirs() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("src/.shadow")).unwrap();
        std::fs::write(base.path().join("src/.shadow/db"), b"secret").unwrap();
        std::fs::write(base.path().join("src/keep.txt"), b"keep").unwrap();

        let out = base.path().join("out");
        let (count, _) = materialize(&base.path().join("src"), &out, CowStrategy::Copy).unwrap();
        assert_eq!(count, 1);
        assert!(!out.join(".shadow").exists());
    }

    #[test]
    fn materialize_with_reports_each_file() {
        let base = tempfile::tempdir().unwrap();
        let src = base.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("b.txt"), b"b").unwrap();

        let out = base.path().join("out");
        let mut seen = Vec::new();
        let (count, _) = materialize_with(&src, &out, CowStrategy::Copy, |_, rel| {
            seen.push(rel.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
        seen.sort();
        assert_eq!(seen, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn tree_stats_matches_copy_set_and_sums_sizes() {
        let base = tempfile::tempdir().unwrap();
        let src = base.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(src.join("target")).unwrap();
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join("a.txt"), b"12345").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"abc").unwrap();
        std::fs::write(src.join("target/big.o"), b"should be ignored").unwrap();
        std::fs::write(src.join(".git/config"), b"ignored").unwrap();

        let (bytes, files) = tree_stats(&src).unwrap();
        assert_eq!(files, 2);
        assert_eq!(bytes, 8);
    }
}
