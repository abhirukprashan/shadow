// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! End-to-end integration tests exercising the public `shadow` API across
//! workspace creation, diffing, conflict detection and atomic merging.

use std::fs;
use std::path::Path;

use shadow::engine::workspace::Engine;
use shadow::merger::conflicts;
use shadow::storage::models::WorkspaceStatus;

/// Create a fresh project root containing `files` (relative path -> contents).
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, data) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }
    dir
}

#[test]
fn init_creates_engine_layout() {
    let proj = project(&[]);
    let _engine = Engine::open(proj.path()).unwrap();
    assert!(proj.path().join(".shadow").is_dir());
    assert!(proj.path().join(".shadow-workspaces").is_dir());
    assert!(proj.path().join(".shadow/staging").is_dir());
}

#[test]
fn cow_creation_seeds_workspace_without_touching_root() {
    let proj = project(&[("src/main.rs", "fn main() {}"), ("README.md", "hi")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("feature", None).unwrap();

    // Workspace contains a real physical copy of the base files.
    assert!(Path::new(&ws.path).join("src/main.rs").exists());
    assert!(Path::new(&ws.path).join("README.md").exists());

    // No engine internals leaked into the workspace.
    assert!(!Path::new(&ws.path).join(".shadow").exists());

    // Mutating the workspace leaves the root untouched (CoW isolation).
    fs::write(Path::new(&ws.path).join("README.md"), "changed in ws").unwrap();
    assert_eq!(
        fs::read_to_string(proj.path().join("README.md")).unwrap(),
        "hi"
    );
}

#[test]
fn stacked_workspace_tracks_parent_lineage() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    let parent = engine.create("base", None).unwrap();
    let child = engine.create("stacked", Some("base")).unwrap();
    assert_eq!(child.parent_id, Some(parent.id));
    assert!(Path::new(&child.path).join("a.txt").exists());
}

#[test]
fn diff_reports_changes_against_baseline() {
    let proj = project(&[("keep.txt", "k"), ("mod.txt", "old"), ("gone.txt", "x")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    let ws_path = Path::new(&ws.path).to_path_buf();

    fs::write(ws_path.join("mod.txt"), "new").unwrap();
    fs::remove_file(ws_path.join("gone.txt")).unwrap();
    fs::write(ws_path.join("added.txt"), "brand new").unwrap();

    let base = engine.baseline(&ws).unwrap();
    let changes = shadow::engine::snapshot::diff(&base, &engine.workspace_manifest(&ws).unwrap());
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&"mod.txt"));
    assert!(paths.contains(&"gone.txt"));
    assert!(paths.contains(&"added.txt"));
    assert!(!paths.contains(&"keep.txt"));
}

#[test]
fn merge_applies_changes_atomically_to_root() {
    let proj = project(&[("src/lib.rs", "old")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    fs::write(Path::new(&ws.path).join("src/lib.rs"), "new").unwrap();
    fs::write(Path::new(&ws.path).join("extra.txt"), "extra").unwrap();

    let report = conflicts::promote(&engine, "dev").unwrap();
    assert!(report.clean);
    assert_eq!(
        fs::read_to_string(proj.path().join("src/lib.rs")).unwrap(),
        "new"
    );
    assert_eq!(
        fs::read_to_string(proj.path().join("extra.txt")).unwrap(),
        "extra"
    );
    // Workspace is consumed: no longer resolvable, and its directory is gone,
    // but the metadata record survives flagged `Merged` as project history.
    assert!(engine.find("dev").is_err());
    assert!(!Path::new(&ws.path).exists());
    let all = engine.list().unwrap();
    assert!(
        all.iter()
            .any(|w| w.name == "dev" && w.status == WorkspaceStatus::Merged)
    );
}

#[test]
fn reconcile_keeps_merged_records() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    fs::write(Path::new(&ws.path).join("a.txt"), "merged").unwrap();

    assert!(conflicts::promote(&engine, "dev").unwrap().clean);

    // A merged record has no directory by design; reconciliation must not
    // mistake it for a ghost and prune it.
    let report = engine.reconcile().unwrap();
    assert_eq!(report.pruned, 0);
    let all = engine.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, WorkspaceStatus::Merged);
}

#[test]
fn merge_aborts_on_conflict_and_preserves_root() {
    let proj = project(&[("f.txt", "base")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();

    // Both sides change the same file differently.
    fs::write(Path::new(&ws.path).join("f.txt"), "workspace").unwrap();
    fs::write(proj.path().join("f.txt"), "root").unwrap();

    let report = conflicts::promote(&engine, "dev").unwrap();
    assert!(report.has_conflicts());
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].path, "f.txt");
    // A machine-readable manifest is emitted for agents/IDEs.
    let json = report.conflicts_json.as_ref().expect("conflicts.json path");
    assert!(json.exists());

    // Root preserved, workspace preserved so the user can resolve.
    assert_eq!(
        fs::read_to_string(proj.path().join("f.txt")).unwrap(),
        "root"
    );
    assert!(engine.find("dev").is_ok());
}

#[test]
fn promotion_auto_merges_non_overlapping_edits() {
    let base = "one\ntwo\nthree\nfour\nfive\n";
    let proj = project(&[("f.txt", base)]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    let ws_path = Path::new(&ws.path).to_path_buf();

    // Workspace edits the first line; root edits the last line.
    fs::write(ws_path.join("f.txt"), "ONE\ntwo\nthree\nfour\nfive\n").unwrap();
    fs::write(proj.path().join("f.txt"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();

    let report = conflicts::promote(&engine, "dev").unwrap();
    assert!(report.clean, "non-overlapping hunks should auto-merge");
    let merged = fs::read_to_string(proj.path().join("f.txt")).unwrap();
    assert!(merged.contains("ONE"));
    assert!(merged.contains("FIVE"));
    // No conflict markers leaked into the merged file.
    assert!(!merged.contains("<<<<<<<"));
}

#[test]
fn promotion_writes_git_conflict_markers_on_overlap() {
    let base = "one\ntwo\nthree\n";
    let proj = project(&[("f.txt", base)]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    let ws_path = Path::new(&ws.path).to_path_buf();

    fs::write(ws_path.join("f.txt"), "one\nOURS\nthree\n").unwrap();
    fs::write(proj.path().join("f.txt"), "one\nTHEIRS\nthree\n").unwrap();

    let report = conflicts::promote(&engine, "dev").unwrap();
    assert!(report.has_conflicts());
    let staged = fs::read_to_string(&report.conflicts[0].staged_path).unwrap();
    assert!(staged.contains("<<<<<<<"));
    assert!(staged.contains("======="));
    assert!(staged.contains(">>>>>>>"));
    // Root remains pristine (no markers written there).
    assert_eq!(
        fs::read_to_string(proj.path().join("f.txt")).unwrap(),
        "one\nTHEIRS\nthree\n"
    );
}

#[test]
fn integration_merges_two_workspaces_into_new_one() {
    let proj = project(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let engine = Engine::open(proj.path()).unwrap();
    let alpha = engine.create("alpha", None).unwrap();
    let beta = engine.create("beta", None).unwrap();

    fs::write(Path::new(&alpha.path).join("a.txt"), "a-alpha\n").unwrap();
    fs::write(Path::new(&beta.path).join("b.txt"), "b-beta\n").unwrap();

    let report = conflicts::integrate(&engine, "alpha", "beta", "combined").unwrap();
    assert!(report.clean);
    assert_eq!(report.output_workspace.as_deref(), Some("combined"));

    let combined = engine.find("combined").unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(&combined.path).join("a.txt")).unwrap(),
        "a-alpha\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&combined.path).join("b.txt")).unwrap(),
        "b-beta\n"
    );

    // Sources are untouched and remain discoverable.
    assert!(engine.find("alpha").is_ok());
    assert!(engine.find("beta").is_ok());
    assert_eq!(
        fs::read_to_string(proj.path().join("a.txt")).unwrap(),
        "a\n"
    );
}

#[test]
fn reconcile_prunes_ghost_metadata() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("ghost", None).unwrap();

    // Simulate a user running `rm -rf` on the workspace directory.
    fs::remove_dir_all(&ws.path).unwrap();

    let report = engine.reconcile().unwrap();
    assert_eq!(report.pruned, 1);
    assert!(engine.find("ghost").is_err());
}

#[test]
fn dropping_parent_detaches_children_without_data_loss() {
    let proj = project(&[("shared.txt", "data")]);
    let engine = Engine::open(proj.path()).unwrap();
    let parent = engine.create("parent", None).unwrap();
    let child = engine.create("child", Some("parent")).unwrap();
    let child_path = Path::new(&child.path).to_path_buf();

    engine.drop("parent").unwrap();

    // Child survives with its data and is detached from the dropped parent.
    assert!(child_path.join("shared.txt").exists());
    let still = engine.find("child").unwrap();
    assert_eq!(still.parent_id, None);
    let _ = parent;
}

#[test]
fn heartbeat_marks_abandoned_workspace() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("live", None).unwrap();
    assert!(engine.is_alive(&ws));

    // Remove the lock file to simulate an expired/absent heartbeat.
    fs::remove_file(Path::new(&ws.path).join(".shadow-lock")).unwrap();
    assert!(!engine.is_alive(&ws));
}

#[test]
fn promotion_rejects_traversing_manifest_keys() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    fs::write(Path::new(&ws.path).join("a.txt"), "changed").unwrap();

    // Pretend the workspace came from someone else. The baseline manifest is
    // a plain file, so an attacker can slip `../PWNED.txt` into it.
    let manifest_path = Path::new(&ws.path).join(".shadow-baseline.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    v["../PWNED.txt"] = serde_json::json!({ "size": 0, "hash": "0".repeat(64) });
    fs::write(&manifest_path, serde_json::to_vec(&v).unwrap()).unwrap();

    let err = conflicts::promote(&engine, "dev").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("unsafe path"), "unexpected error: {text}");
    assert!(!proj.path().join("../PWNED.txt").exists());
    assert!(!proj.path().join("PWNED.txt").exists());
}

#[test]
fn name_traversal_is_rejected() {
    let proj = project(&[("a.txt", "a")]);
    let engine = Engine::open(proj.path()).unwrap();
    assert!(engine.create("../../tmp/evil", None).is_err());
    assert!(engine.create("a/b", None).is_err());
    assert!(engine.find("../x").is_err());
    let escapes = proj
        .path()
        .join(".shadow-workspaces")
        .read_dir()
        .unwrap()
        .count();
    assert_eq!(escapes, 0);
}

#[test]
fn promotion_never_follows_workspace_symlinks() {
    let proj = project(&[("a.txt", "a")]);
    // A file outside the project that a malicious workspace will try to leak.
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("secret.txt");
    fs::write(&secret, "TOP SECRET").unwrap();

    let engine = Engine::open(proj.path()).unwrap();
    let ws = engine.create("dev", None).unwrap();
    fs::write(Path::new(&ws.path).join("a.txt"), "changed").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, Path::new(&ws.path).join("leak.txt")).unwrap();

    // The manifest walk skips symlinks, so add an entry by hand to force
    // `leak.txt` into the merge decisions.
    let manifest_path = Path::new(&ws.path).join(".shadow-baseline.json");
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    v["leak.txt"] = serde_json::json!({ "size": 10, "hash": "x".repeat(64) });
    fs::write(&manifest_path, serde_json::to_vec(&v).unwrap()).unwrap();

    let report = conflicts::promote(&engine, "dev").unwrap();
    assert!(report.clean);
    let leak = proj.path().join("leak.txt");
    assert!(!leak.exists(), "symlink content was exfiltrated!");
    if leak.exists() {
        assert_ne!(fs::read_to_string(&leak).unwrap(), "TOP SECRET");
    }
}
