// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Line-level three-way (diff3) merging with Git-compatible conflict markers.
//!
//! Given a shared `base` and two derived versions (`ours`, `theirs`) the merge
//! is performed hunk-by-hunk:
//!
//! * changes to non-overlapping line ranges are merged automatically;
//! * changes to overlapping ranges are reported as [`ConflictHunk`]s and, when
//!   materialised, written using standard `<<<<<<<` / `=======` / `>>>>>>>`
//!   markers so that native IDE merge UIs light up.
//!
//! Lines are represented as `&str` slices borrowed from the three input
//! buffers (output lines are `Cow<str>`), so merging a file performs no
//! per-line heap allocations — only the final joined output and any conflict
//! marker lines are ever allocated.

use serde::Serialize;
use std::borrow::Cow;

/// A conflicting region, expressed as 1-based inclusive line ranges in each of
/// the three inputs, together with the competing snippets.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConflictHunk {
    pub base_start: usize,
    pub base_end: usize,
    pub ours_start: usize,
    pub ours_end: usize,
    pub theirs_start: usize,
    pub theirs_end: usize,
    pub base_snippet: String,
    pub ours_snippet: String,
    pub theirs_snippet: String,
}

/// The result of merging one file's lines.
#[derive(Debug, Clone)]
pub enum MergeOutcome<'a> {
    /// No overlapping changes; the merged lines are conflict-free.
    Clean(Vec<Cow<'a, str>>),
    /// Overlapping changes were found. `lines` contains the merged content with
    /// Git conflict markers already injected.
    Conflicted {
        lines: Vec<Cow<'a, str>>,
        conflicts: Vec<ConflictHunk>,
    },
}

impl<'a> MergeOutcome<'a> {
    pub fn is_clean(&self) -> bool {
        matches!(self, MergeOutcome::Clean(_))
    }

    pub fn into_lines(self) -> Vec<Cow<'a, str>> {
        match self {
            MergeOutcome::Clean(lines) => lines,
            MergeOutcome::Conflicted { lines, .. } => lines,
        }
    }
}

/// A contiguous run of changes from `base` to a derived version.
#[derive(Debug, Clone)]
struct Hunk<'a> {
    /// Inclusive start index in `base`.
    base_start: usize,
    /// Exclusive end index in `base` (equals `base_start` for pure insertions).
    base_end: usize,
    /// Replacement lines contributed by the derived version.
    lines: Vec<Cow<'a, str>>,
}

fn diff_hunks<'a>(base: &[&'a str], other: &[&'a str]) -> Vec<Hunk<'a>> {
    let mut hunks: Vec<Hunk<'a>> = Vec::new();
    let mut base_idx = 0usize;
    let mut current: Option<Hunk<'a>> = None;

    for item in diff::slice(base, other) {
        match item {
            diff::Result::Both(_, _) => {
                if let Some(h) = current.take() {
                    hunks.push(h);
                }
                base_idx += 1;
            }
            diff::Result::Left(_) => {
                let h = current.get_or_insert_with(|| Hunk {
                    base_start: base_idx,
                    base_end: base_idx,
                    lines: Vec::new(),
                });
                h.base_end = base_idx + 1;
                base_idx += 1;
            }
            diff::Result::Right(line) => {
                let h = current.get_or_insert_with(|| Hunk {
                    base_start: base_idx,
                    base_end: base_idx,
                    lines: Vec::new(),
                });
                h.lines.push(Cow::Borrowed(*line));
            }
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    hunks
}

/// Whether `h` belongs to the cluster spanning `[start, end)`.
fn in_cluster(h: &Hunk, start: usize, end: usize, cluster_empty: bool) -> bool {
    if cluster_empty {
        return h.base_start == start;
    }
    if h.base_start < end {
        return true;
    }
    // A zero-width insertion exactly at the cluster boundary joins only a
    // zero-width cluster (i.e. another insertion at the same point).
    h.base_start == end && h.base_end == end && start == end
}

/// Reconstruct the derived lines for the base range `[start, end)` by applying
/// `hunks` (which must be ordered and non-overlapping among themselves).
fn reconstruct<'a>(
    base: &[&'a str],
    start: usize,
    end: usize,
    hunks: &[&Hunk<'a>],
) -> Vec<Cow<'a, str>> {
    let mut out = Vec::new();
    let mut pos = start;
    for h in hunks {
        if h.base_start > pos {
            out.extend(base[pos..h.base_start].iter().map(|s| Cow::Borrowed(*s)));
        }
        out.extend(h.lines.iter().cloned());
        pos = pos.max(h.base_end);
    }
    if pos < end {
        out.extend(base[pos..end].iter().map(|s| Cow::Borrowed(*s)));
    }
    out
}

/// Map a base index to the corresponding index in a derived version.
fn project_index(pos: usize, hunks: &[Hunk]) -> usize {
    let mut delta: isize = 0;
    for h in hunks {
        if h.base_end <= pos {
            delta += h.lines.len() as isize - (h.base_end - h.base_start) as isize;
        } else {
            break;
        }
    }
    (pos as isize + delta).max(0) as usize
}

/// Build the Git-style conflict markers for a single conflicting region.
pub fn conflict_markers<'a>(
    ours_label: &str,
    theirs_label: &str,
    ours: &[Cow<'a, str>],
    theirs: &[Cow<'a, str>],
) -> Vec<Cow<'a, str>> {
    let mut out = Vec::with_capacity(ours.len() + theirs.len() + 3);
    out.push(Cow::Owned(format!("<<<<<<< {ours_label}")));
    out.extend_from_slice(ours);
    out.push(Cow::Owned("=======".to_string()));
    out.extend_from_slice(theirs);
    out.push(Cow::Owned(format!(">>>>>>> {theirs_label}")));
    out
}

/// Perform a three-way line merge.
pub fn merge_lines<'a>(
    base: &[&'a str],
    ours: &[&'a str],
    theirs: &[&'a str],
    ours_label: &str,
    theirs_label: &str,
) -> MergeOutcome<'a> {
    let ours_hunks = diff_hunks(base, ours);
    let theirs_hunks = diff_hunks(base, theirs);

    let mut out: Vec<Cow<'a, str>> = Vec::new();
    let mut conflicts: Vec<ConflictHunk> = Vec::new();

    let (mut oi, mut ti) = (0usize, 0usize);
    let mut pos = 0usize;
    let n = base.len();

    while oi < ours_hunks.len() || ti < theirs_hunks.len() {
        let next_ours = ours_hunks
            .get(oi)
            .map(|h| h.base_start)
            .unwrap_or(usize::MAX);
        let next_theirs = theirs_hunks
            .get(ti)
            .map(|h| h.base_start)
            .unwrap_or(usize::MAX);
        let next = next_ours.min(next_theirs).min(n);

        if next > pos {
            out.extend(base[pos..next].iter().map(|s| Cow::Borrowed(*s)));
            pos = next;
        }

        let cluster_start = next;
        let mut cluster_end = next;
        let mut clustered_ours: Vec<&Hunk> = Vec::new();
        let mut clustered_theirs: Vec<&Hunk> = Vec::new();

        loop {
            let mut progressed = false;
            while let Some(h) = ours_hunks.get(oi) {
                if in_cluster(
                    h,
                    cluster_start,
                    cluster_end,
                    clustered_ours.is_empty() && clustered_theirs.is_empty(),
                ) {
                    cluster_end = cluster_end.max(h.base_end);
                    clustered_ours.push(h);
                    oi += 1;
                    progressed = true;
                } else {
                    break;
                }
            }
            while let Some(h) = theirs_hunks.get(ti) {
                if in_cluster(
                    h,
                    cluster_start,
                    cluster_end,
                    clustered_ours.is_empty() && clustered_theirs.is_empty(),
                ) {
                    cluster_end = cluster_end.max(h.base_end);
                    clustered_theirs.push(h);
                    ti += 1;
                    progressed = true;
                } else {
                    break;
                }
            }
            if !progressed {
                break;
            }
        }

        let ours_lines = reconstruct(base, cluster_start, cluster_end, &clustered_ours);
        let theirs_lines = reconstruct(base, cluster_start, cluster_end, &clustered_theirs);
        let ours_changed = !clustered_ours.is_empty();
        let theirs_changed = !clustered_theirs.is_empty();

        if !ours_changed {
            out.extend(theirs_lines);
        } else if !theirs_changed || ours_lines == theirs_lines {
            out.extend(ours_lines);
        } else {
            let base_slice = &base[cluster_start..cluster_end];
            let ours_start = project_index(cluster_start, &ours_hunks) + 1;
            let ours_end = ours_start + ours_lines.len().saturating_sub(1);
            let theirs_start = project_index(cluster_start, &theirs_hunks) + 1;
            let theirs_end = theirs_start + theirs_lines.len().saturating_sub(1);
            conflicts.push(ConflictHunk {
                base_start: cluster_start + 1,
                base_end: if cluster_end == cluster_start {
                    cluster_start
                } else {
                    cluster_end
                },
                ours_start,
                ours_end,
                theirs_start,
                theirs_end,
                base_snippet: base_slice.join("\n"),
                ours_snippet: ours_lines.join("\n"),
                theirs_snippet: theirs_lines.join("\n"),
            });
            out.extend(conflict_markers(
                ours_label,
                theirs_label,
                &ours_lines,
                &theirs_lines,
            ));
        }

        pos = cluster_end.max(pos);
    }

    if pos < n {
        out.extend(base[pos..].iter().map(|s| Cow::Borrowed(*s)));
    }

    if conflicts.is_empty() {
        MergeOutcome::Clean(out)
    } else {
        MergeOutcome::Conflicted {
            lines: out,
            conflicts,
        }
    }
}

/// Whether `bytes` look like text (valid UTF-8 with no NUL bytes).
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Split file content into lines in a way that round-trips exactly through
/// [`join_lines`]. Borrows from `content` — no allocations.
pub fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    }
}

/// Join merged lines back into file content with a single allocation.
pub fn join_lines(lines: &[Cow<'_, str>]) -> String {
    let mut total = 0usize;
    for l in lines {
        total += l.len() + 1;
    }
    let mut out = String::with_capacity(total.saturating_sub(1));
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(lines: &[Cow<'_, str>]) -> String {
        join_lines(lines)
    }

    #[test]
    fn non_overlapping_changes_auto_merge() {
        let base = ["a", "b", "c", "d", "e"];
        let ours = ["A", "b", "c", "d", "e"];
        let theirs = ["a", "b", "c", "d", "E"];
        match merge_lines(&base, &ours, &theirs, "ours", "theirs") {
            MergeOutcome::Clean(lines) => assert_eq!(joined(&lines), "A\nb\nc\nd\nE"),
            other => panic!("expected clean merge, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_changes_conflict_with_markers() {
        let base = ["a", "b", "c"];
        let ours = ["a", "OURS", "c"];
        let theirs = ["a", "THEIRS", "c"];
        match merge_lines(&base, &ours, &theirs, "ws", "main") {
            MergeOutcome::Conflicted { lines, conflicts } => {
                assert_eq!(conflicts.len(), 1);
                let joined = joined(&lines);
                assert!(joined.contains("<<<<<<< ws"));
                assert!(joined.contains("======="));
                assert!(joined.contains(">>>>>>> main"));
                let c = &conflicts[0];
                assert_eq!(c.base_snippet, "b");
                assert_eq!(c.ours_snippet, "OURS");
                assert_eq!(c.theirs_snippet, "THEIRS");
            }
            _ => panic!("expected conflict"),
        }
    }

    #[test]
    fn identical_changes_do_not_conflict() {
        let base = ["a", "b"];
        let ours = ["a", "X"];
        let theirs = ["a", "X"];
        assert!(merge_lines(&base, &ours, &theirs, "o", "t").is_clean());
    }

    #[test]
    fn disjoint_insertions_merge() {
        let base = ["one", "two"];
        let ours = ["start", "one", "two"];
        let theirs = ["one", "two", "end"];
        match merge_lines(&base, &ours, &theirs, "o", "t") {
            MergeOutcome::Clean(lines) => {
                assert_eq!(joined(&lines), "start\none\ntwo\nend");
            }
            _ => panic!("expected clean merge"),
        }
    }

    #[test]
    fn split_and_join_round_trip() {
        let content = "a\nb\n\n";
        let lines = split_lines(content);
        assert_eq!(join_lines(&lines.iter().map(|s| Cow::Borrowed(*s)).collect::<Vec<_>>()), content);
        assert!(split_lines("").is_empty());
    }
}
