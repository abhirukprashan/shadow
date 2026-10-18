// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Content manifests used for fast diffing, 3-way comparison and conflict
//! detection.
//!
//! A manifest is a map of workspace-relative path -> [`FileEntry`] capturing a
//! file's size and content hash. The manifest taken at workspace creation time
//! acts as the 3-way merge *base*, while a freshly computed manifest supplies
//! "ours" (workspace) and "theirs" (project root).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use walkdir::WalkDir;

use crate::co_w::cow::{RESERVED_DIRS, RESERVED_FILES};

/// Metadata about a single tracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub size: u64,
    pub hash: String,
}

/// A snapshot of relative path -> file entry.
pub type Manifest = BTreeMap<String, FileEntry>;

fn is_reserved(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    RESERVED_DIRS.contains(&first) || RESERVED_FILES.contains(&first)
}

/// Compute a manifest for the tree rooted at `root`, skipping engine-internal
/// files and directories.
pub fn compute(root: &Path) -> Result<Manifest> {
    let mut manifest = Manifest::new();
    if !root.exists() {
        return Ok(manifest);
    }
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Keep the root; prune reserved subtrees so we never walk the build
            // tree or engine state.
            if e.depth() == 0 {
                return true;
            }
            let rel = e.path().strip_prefix(root).unwrap_or(e.path());
            !RESERVED_DIRS
                .iter()
                .any(|r| e.file_name().to_str() == Some(r) && e.file_type().is_dir())
                && !is_reserved(&rel.to_string_lossy())
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_reserved(&rel_str) {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = hex::encode(hasher.finalize());
        manifest.insert(
            rel_str,
            FileEntry {
                size: bytes.len() as u64,
                hash,
            },
        );
    }
    Ok(manifest)
}

/// The classification of a path between two manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A single entry in a diff.
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

/// Diff two manifests, reporting entries that differ between `base` and `other`.
pub fn diff(base: &Manifest, other: &Manifest) -> Vec<Change> {
    let mut changes = Vec::new();
    for (path, entry) in other {
        match base.get(path) {
            None => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Added,
            }),
            Some(old) if old.hash != entry.hash => changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Modified,
            }),
            _ => {}
        }
    }
    for path in base.keys() {
        if !other.contains_key(path) {
            changes.push(Change {
                path: path.clone(),
                kind: ChangeKind::Deleted,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// Detect 3-way merge conflicts.
///
/// A conflict exists when both the project root ("theirs") and the workspace
/// ("ours") changed a path relative to the shared baseline, and their resulting
/// contents differ.
pub fn detect_conflicts(base: &Manifest, ours: &Manifest, theirs: &Manifest) -> Vec<String> {
    let mut conflicts = Vec::new();
    for (path, ours_entry) in ours {
        let base_entry = base.get(path);
        let theirs_entry = theirs.get(path);

        let ours_changed = base_entry != Some(ours_entry);
        let theirs_changed = base_entry != theirs_entry;

        if ours_changed && theirs_changed && theirs_entry != Some(ours_entry) {
            conflicts.push(path.clone());
        }
    }
    conflicts.sort();
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, data: &[u8]) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, data).unwrap();
    }

    #[test]
    fn detects_added_modified_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "keep.txt", b"same");
        write(dir.path(), "mod.txt", b"old");
        write(dir.path(), "gone.txt", b"bye");
        let base = compute(dir.path()).unwrap();

        fs::write(dir.path().join("mod.txt"), b"new").unwrap();
        fs::remove_file(dir.path().join("gone.txt")).unwrap();
        write(dir.path(), "added.txt", b"hi");
        let other = compute(dir.path()).unwrap();

        let changes = diff(&base, &other);
        let kinds: Vec<_> = changes.iter().map(|c| (c.kind, c.path.as_str())).collect();
        assert!(kinds.contains(&(ChangeKind::Added, "added.txt")));
        assert!(kinds.contains(&(ChangeKind::Modified, "mod.txt")));
        assert!(kinds.contains(&(ChangeKind::Deleted, "gone.txt")));
        assert!(!kinds.iter().any(|(_, p)| *p == "keep.txt"));
    }

    #[test]
    fn detects_conflict_when_both_sides_change() {
        let mut base = Manifest::new();
        base.insert(
            "f".into(),
            FileEntry {
                size: 1,
                hash: "base".into(),
            },
        );
        let mut ours = Manifest::new();
        ours.insert(
            "f".into(),
            FileEntry {
                size: 1,
                hash: "ours".into(),
            },
        );
        let mut theirs = Manifest::new();
        theirs.insert(
            "f".into(),
            FileEntry {
                size: 1,
                hash: "theirs".into(),
            },
        );
        assert_eq!(
            detect_conflicts(&base, &ours, &theirs),
            vec!["f".to_string()]
        );
    }

    #[test]
    fn no_conflict_when_only_one_side_changes() {
        let mut base = Manifest::new();
        base.insert(
            "f".into(),
            FileEntry {
                size: 1,
                hash: "base".into(),
            },
        );
        let ours = base.clone();
        let mut theirs = base.clone();
        theirs.insert(
            "f".into(),
            FileEntry {
                size: 1,
                hash: "theirs".into(),
            },
        );
        assert!(detect_conflicts(&base, &ours, &theirs).is_empty());
    }
}
