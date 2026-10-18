// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Merge orchestration for both merge modes.
//!
//! * **Promotion** (`shadow merge <ws>`): squash the workspace HEAD state into
//!   the project root, staged in `.shadow/staging/` and applied atomically.
//! * **Integration** (`shadow merge <a> <b> --out <new>`): combine the HEAD
//!   states of two finished workspaces into a brand-new CoW workspace without
//!   mutating the sources.
//!
//! Both paths run a line-level diff3 merge. Non-overlapping changes are merged
//! automatically; overlapping changes abort the merge, keep the target pristine
//! and are delivered either to a TTY or as a machine-readable
//! `.shadow/conflicts.json` manifest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::co_w::cow::RESERVED_FILES;
use crate::engine::snapshot::{self, Manifest};
use crate::engine::workspace::Engine;
use crate::merger::diff3::{self, ConflictHunk, MergeOutcome};
use crate::merger::merger;
use crate::storage::models::OperationLog;

/// Which flavour of merge is being performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    Promotion,
    Integration,
}

impl MergeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MergeKind::Promotion => "promotion",
            MergeKind::Integration => "integration",
        }
    }
}

/// A conflict in a single file, with the staged path containing markers.
#[derive(Debug, Clone)]
pub struct FileConflict {
    pub path: String,
    pub staged_path: PathBuf,
    pub hunks: Vec<ConflictHunk>,
}

/// The outcome of a merge attempt.
#[derive(Debug, Clone)]
pub struct MergeReport {
    pub kind: MergeKind,
    pub clean: bool,
    pub written: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<FileConflict>,
    /// Name of the newly created workspace (integration merges only).
    pub output_workspace: Option<String>,
    /// Path of the generated `conflicts.json` when the merge conflicted.
    pub conflicts_json: Option<PathBuf>,
}

impl MergeReport {
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// What to do with a single path.
enum Decision {
    Write(String, Vec<u8>),
    Delete(String),
    Conflict(String, Vec<ConflictHunk>, Option<Vec<u8>>),
}

/// Read a file from a workspace-relative path, returning `None` if absent.
fn read_opt(dir: &Path, rel: &str) -> Result<Option<Vec<u8>>> {
    let path = dir.join(rel);
    if path.is_file() {
        Ok(Some(std::fs::read(path)?))
    } else {
        Ok(None)
    }
}

/// Three-way merge of a single path's contents.
fn merge_path(
    path: &str,
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
    ours_label: &str,
    theirs_label: &str,
) -> Decision {
    // Deletion handling.
    match ours {
        None => {
            match (base, theirs) {
                // Both removed, or ours removed and theirs unchanged.
                (_, None) => Decision::Delete(path.to_string()),
                (None, Some(t)) => {
                    // Added on theirs side only.
                    Decision::Write(path.to_string(), t.to_vec())
                }
                (Some(b), Some(t)) if b == t => Decision::Delete(path.to_string()),
                // Deleted on our side but modified on theirs.
                _ => Decision::Conflict(path.to_string(), binary_hunks(), None),
            }
        }
        Some(o) => {
            if let Some(t) = theirs {
                if o == t {
                    return Decision::Write(path.to_string(), o.to_vec());
                }
                if base == Some(o) {
                    // We did not change it; take theirs.
                    return Decision::Write(path.to_string(), t.to_vec());
                }
                if base == Some(t) {
                    // They did not change it; take ours.
                    return Decision::Write(path.to_string(), o.to_vec());
                }

                // Both changed differently: attempt a line merge for text.
                let text_mergeable = diff3::is_text(o)
                    && diff3::is_text(t)
                    && base.map(diff3::is_text).unwrap_or(false);
                if text_mergeable {
                    let b = base.unwrap_or(&[]);
                    let base_lines = diff3::split_lines(&String::from_utf8_lossy(b));
                    let ours_lines = diff3::split_lines(&String::from_utf8_lossy(o));
                    let theirs_lines = diff3::split_lines(&String::from_utf8_lossy(t));
                    match diff3::merge_lines(
                        &base_lines,
                        &ours_lines,
                        &theirs_lines,
                        ours_label,
                        theirs_label,
                    ) {
                        MergeOutcome::Clean(lines) => {
                            return Decision::Write(
                                path.to_string(),
                                diff3::join_lines(&lines).into_bytes(),
                            );
                        }
                        MergeOutcome::Conflicted { lines, conflicts } => {
                            return Decision::Conflict(
                                path.to_string(),
                                conflicts,
                                Some(diff3::join_lines(&lines).into_bytes()),
                            );
                        }
                    }
                }

                // Binary (or non-UTF-8) conflict: keep our bytes for inspection.
                Decision::Conflict(path.to_string(), binary_hunks(), Some(o.to_vec()))
            } else if base == Some(o) {
                // We did not change it and theirs deleted it: propagate delete.
                Decision::Delete(path.to_string())
            } else if base.is_none() {
                // Added only on our side.
                Decision::Write(path.to_string(), o.to_vec())
            } else {
                // Theirs deleted but we modified it.
                Decision::Conflict(path.to_string(), binary_hunks(), Some(o.to_vec()))
            }
        }
    }
}

fn binary_hunks() -> Vec<ConflictHunk> {
    vec![ConflictHunk {
        base_start: 1,
        base_end: 1,
        ours_start: 1,
        ours_end: 1,
        theirs_start: 1,
        theirs_end: 1,
        base_snippet: String::new(),
        ours_snippet: "<binary or non-text content>".into(),
        theirs_snippet: "<binary or non-text content>".into(),
    }]
}

/// Evaluate every path that either side changed.
fn decisions(
    base_dir: &Path,
    ours_dir: &Path,
    theirs_dir: &Path,
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
    ours_label: &str,
    theirs_label: &str,
) -> Result<Vec<Decision>> {
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for c in snapshot::diff(base, ours) {
        paths.insert(c.path);
    }
    for c in snapshot::diff(base, theirs) {
        paths.insert(c.path);
    }

    let mut out = Vec::new();
    for path in paths {
        let b = read_opt(base_dir, &path)?;
        let o = read_opt(ours_dir, &path)?;
        let t = read_opt(theirs_dir, &path)?;
        out.push(merge_path(
            &path,
            b.as_deref(),
            o.as_deref(),
            t.as_deref(),
            ours_label,
            theirs_label,
        ));
    }
    Ok(out)
}

/// Write a decision's content into `dir` at its relative path.
fn write_into(dir: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let target = dir.join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, bytes)?;
    Ok(())
}

fn remove_from(dir: &Path, rel: &str) {
    let target = dir.join(rel);
    if target.exists() {
        let _ = std::fs::remove_file(target);
    }
}

/// Promotion merge: squash a workspace's HEAD into the project root.
pub fn promote(engine: &Engine, name: &str) -> Result<MergeReport> {
    let ws = engine.find(name)?;
    let base_dir = engine.baseline_dir(&ws);
    let ours_dir = PathBuf::from(&ws.path);
    let theirs_dir = engine.root().to_path_buf();

    let base = engine.baseline(&ws)?;
    let ours = engine.workspace_manifest(&ws)?;
    let theirs = engine.root_manifest()?;

    let decisions = decisions(
        &base_dir,
        &ours_dir,
        &theirs_dir,
        &base,
        &ours,
        &theirs,
        name,
        "root",
    )?;

    let report = finalize(
        engine,
        MergeKind::Promotion,
        &decisions,
        &[name.to_string()],
        "root",
        None,
    )?;

    // A fully promoted workspace is consumed; abandoned otherwise.
    if report.clean {
        engine.finalize_drop(&ws)?;
    }
    Ok(report)
}

/// Shared finalisation: either abort with markers + JSON, or apply atomically.
fn finalize(
    engine: &Engine,
    kind: MergeKind,
    decisions: &[Decision],
    sources: &[String],
    target_label: &str,
    output_workspace: Option<&str>,
) -> Result<MergeReport> {
    let staging = engine.reset_staging()?;

    let mut conflicts: Vec<FileConflict> = Vec::new();
    for d in decisions {
        if let Decision::Conflict(path, hunks, staged) = d {
            let staged_path = staging.join(path);
            if let Some(bytes) = staged {
                write_into(&staging, path, bytes)?;
            }
            conflicts.push(FileConflict {
                path: path.clone(),
                staged_path,
                hunks: hunks.clone(),
            });
        }
    }

    if !conflicts.is_empty() {
        // Keep the target pristine. Persist machine-readable conflicts and
        // leave marker-bearing files in staging for IDE/agent resolution.
        let json_path = engine.conflicts_json_path();
        write_conflicts_json(&json_path, kind, sources, target_label, &conflicts)?;
        let op = OperationLog {
            id: uuid::Uuid::new_v4(),
            operation: format!("{}-conflict", kind.as_str()),
            timestamp: chrono::Utc::now(),
            details: Some(format!(
                "{} conflict(s) in {}",
                conflicts.len(),
                sources.join(", ")
            )),
        };
        engine.db().put_operation(&op)?;
        return Ok(MergeReport {
            kind,
            clean: false,
            written: Vec::new(),
            deleted: Vec::new(),
            conflicts,
            output_workspace: None,
            conflicts_json: Some(json_path),
        });
    }

    // Clean merge: stage writes then apply.
    let mut write_paths = Vec::new();
    let mut delete_paths = Vec::new();
    for d in decisions {
        match d {
            Decision::Write(path, bytes) => {
                write_into(&staging, path, bytes)?;
                write_paths.push(path.clone());
            }
            Decision::Delete(path) => delete_paths.push(path.clone()),
            Decision::Conflict(..) => unreachable!("conflicts handled above"),
        }
    }

    match kind {
        MergeKind::Promotion => {
            merger::apply_paths(&staging, engine.root(), &write_paths)?;
            for rel in &delete_paths {
                remove_from(engine.root(), rel);
            }
        }
        MergeKind::Integration => {
            let out_name = output_workspace
                .ok_or_else(|| anyhow::anyhow!("integration merge requires --out"))?;
            let out = engine.create(out_name, None)?;
            let out_dir = PathBuf::from(&out.path);
            for path in &write_paths {
                let staged = staging.join(path);
                let bytes = std::fs::read(&staged)?;
                write_into(&out_dir, path, &bytes)?;
            }
            for rel in &delete_paths {
                remove_from(&out_dir, rel);
            }
        }
    }

    let op = OperationLog {
        id: uuid::Uuid::new_v4(),
        operation: format!("{}-merge", kind.as_str()),
        timestamp: chrono::Utc::now(),
        details: Some(format!(
            "{}: {} written, {} deleted",
            sources.join(", "),
            write_paths.len(),
            delete_paths.len()
        )),
    };
    engine.db().put_operation(&op)?;

    Ok(MergeReport {
        kind,
        clean: true,
        written: write_paths,
        deleted: delete_paths,
        conflicts: Vec::new(),
        output_workspace: output_workspace.map(|s| s.to_string()),
        conflicts_json: None,
    })
}

/// Serialisable manifest written to `.shadow/conflicts.json`.
#[derive(Debug, Serialize)]
struct ConflictManifest {
    kind: String,
    sources: Vec<String>,
    target: String,
    generated_at: String,
    conflicts: Vec<ConflictEntry>,
}

#[derive(Debug, Serialize)]
struct ConflictEntry {
    path: String,
    staged_path: String,
    hunks: Vec<ConflictHunk>,
}

fn write_conflicts_json(
    path: &Path,
    kind: MergeKind,
    sources: &[String],
    target: &str,
    conflicts: &[FileConflict],
) -> Result<()> {
    let manifest = ConflictManifest {
        kind: kind.as_str().to_string(),
        sources: sources.to_vec(),
        target: target.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        conflicts: conflicts
            .iter()
            .map(|c| ConflictEntry {
                path: c.path.clone(),
                staged_path: c.staged_path.to_string_lossy().to_string(),
                hunks: c.hunks.clone(),
            })
            .collect(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("writing conflicts manifest to {}", path.display()))?;
    Ok(())
}

/// Integration merge: combine two workspaces into a new one.
pub fn integrate(engine: &Engine, first: &str, second: &str, out: &str) -> Result<MergeReport> {
    let ws1 = engine.find(first)?;
    let ws2 = engine.find(second)?;

    // The merge base is the first workspace's baseline (both are expected to
    // descend from the same `main` snapshot).
    let base_dir = engine.baseline_dir(&ws1);
    let ours_dir = PathBuf::from(&ws1.path);
    let theirs_dir = PathBuf::from(&ws2.path);

    let base = engine.baseline(&ws1)?;
    let ours = engine.workspace_manifest(&ws1)?;
    let theirs = engine.workspace_manifest(&ws2)?;

    let decisions = decisions(
        &base_dir,
        &ours_dir,
        &theirs_dir,
        &base,
        &ours,
        &theirs,
        first,
        second,
    )?;

    finalize(
        engine,
        MergeKind::Integration,
        &decisions,
        &[first.to_string(), second.to_string()],
        out,
        Some(out),
    )
}

/// Remove a path only if it exists (helper for callers).
pub fn remove_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Reserved files must never be treated as mergeable content.
pub fn is_reserved_file(name: &str) -> bool {
    RESERVED_FILES.contains(&name)
}
