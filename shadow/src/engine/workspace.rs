// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! The central workspace engine.
//!
//! [`Engine`] ties together the CoW primitives, the `redb` metadata store, the
//! heartbeat liveness tracker and the manifest-based snapshotting. It is the
//! single entry point used by the CLI.

use crate::co_w::cow::{self, RESERVED_FILES};
use crate::co_w::fs_utils::CowStrategy;
use crate::engine::heartbeat::Heartbeat;
use crate::engine::snapshot::{self, FileEntry, Manifest};
use crate::storage::db::Db;
use crate::storage::models::{Commit, OperationLog, Workspace, WorkspaceStatus};
use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Name of the sidecar file holding a workspace's baseline manifest.
pub const BASELINE_FILE: &str = ".shadow-baseline.json";

/// Directory inside a workspace holding a reflinked copy of the pristine base
/// tree, used as the merge base for line-level three-way merging.
pub const BASELINE_DIR: &str = ".shadow-baseline";

/// Root-relative locations owned by the engine.
#[derive(Clone)]
pub struct Engine {
    root: PathBuf,
    workspaces_dir: PathBuf,
    staging_dir: PathBuf,
    db_path: PathBuf,
    fs_name: String,
    strategy: CowStrategy,
    db: Db,
}

impl Engine {
    /// Open the engine rooted at `root`, creating engine directories and the
    /// metadata store if they do not yet exist.
    pub fn open(root: &Path) -> Result<Self> {
        let workspaces_dir = root.join(".shadow-workspaces");
        let staging_dir = root.join(".shadow/staging");
        let db_path = root.join(".shadow/metadata.redb");

        // Creating the staging dir also creates its `.shadow` parent.
        fs::create_dir_all(&workspaces_dir)?;
        fs::create_dir_all(&staging_dir)?;

        // One statfs serves both the strategy probe and diagnostics.
        let fs_name = crate::utils::fs::detect_fs_name(root);
        let strategy = CowStrategy::for_fs(&fs_name);
        let db = Db::open(&db_path)?;

        Ok(Self {
            root: root.to_path_buf(),
            workspaces_dir,
            staging_dir,
            db_path,
            fs_name,
            strategy,
            db,
        })
    }

    /// Find the project root for `start` by walking up its ancestors looking
    /// for an existing engine layout (`.shadow/metadata.redb`).
    ///
    /// This lets commands such as `shadow commit` work from inside a workspace
    /// directory, where the current directory is not itself the project root.
    pub fn discover(start: &Path) -> Option<PathBuf> {
        let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        for ancestor in start.ancestors() {
            if ancestor.join(".shadow/metadata.redb").exists() || ancestor.join(".shadow").is_dir()
            {
                return Some(ancestor.to_path_buf());
            }
        }
        None
    }

    /// Open the engine for `start`, using [`Engine::discover`] to locate the
    /// project root, or `start` itself when no existing engine is found.
    pub fn open_discovered(start: &Path) -> Result<Self> {
        let root = Self::discover(start).unwrap_or_else(|| start.to_path_buf());
        Self::open(&root)
    }

    /// The copy strategy in effect for this project.
    pub fn strategy(&self) -> CowStrategy {
        self.strategy
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspaces_dir(&self) -> &Path {
        &self.workspaces_dir
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Path to the machine-readable conflict manifest.
    pub fn conflicts_json_path(&self) -> PathBuf {
        self.root.join(".shadow/conflicts.json")
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Canonical directory name for a workspace (`<name>_<uuid>`).
    fn dir_name(name: &str, id: Uuid) -> String {
        format!("{name}_{id}")
    }

    /// Create a new CoW workspace, optionally stacked on `parent`.
    ///
    /// The base tree (the parent workspace, or the project root when `parent`
    /// is `None`) is materialised via reflinks *and hashed in the same walk*,
    /// so the baseline manifest costs no extra tree traversal.
    pub fn create(&self, name: &str, parent: Option<&str>) -> Result<Workspace> {
        if self.find(name).is_ok() {
            anyhow::bail!("workspace '{name}' already exists");
        }

        let (base_dir, parent_id) = match parent {
            Some(p) => {
                let parent_ws = self.find(p)?;
                (PathBuf::from(&parent_ws.path), Some(parent_ws.id))
            }
            None => (self.root.clone(), None),
        };

        let id = Uuid::new_v4();
        let ws_path = self.workspaces_dir.join(Self::dir_name(name, id));
        fs::create_dir_all(&ws_path)?;

        // Materialise the base tree into the workspace while computing the
        // baseline manifest in the same pass (seed `.shadow` dirs are absent
        // from the base, so nothing reserved is copied).
        let mut base_manifest = Manifest::new();
        cow::materialize_with(
            &base_dir,
            &ws_path,
            self.strategy,
            |src_file, rel| {
                let (hash, size) = crate::utils::fs::hash_file(src_file)?;
                base_manifest.insert(
                    rel.to_string(),
                    FileEntry {
                        size,
                        hash,
                    },
                );
                Ok(())
            },
        )
        .with_context(|| format!("materialising workspace '{name}' from base"))?;

        // Also keep a reflinked copy of the pristine base tree inside the
        // workspace. This is the merge *base* used for line-level diff3
        // merging; reflinks make it effectively free on CoW filesystems.
        let base_snapshot = ws_path.join(BASELINE_DIR);
        cow::materialize(&base_dir, &base_snapshot, self.strategy)
            .with_context(|| format!("recording merge base for '{name}'"))?;

        fs::write(
            ws_path.join(BASELINE_FILE),
            serde_json::to_vec(&base_manifest)?,
        )?;

        let hb = Heartbeat::new(&ws_path);
        hb.write()?;

        let now = Utc::now();
        let ws = Workspace {
            id,
            name: name.to_string(),
            parent_id,
            path: ws_path.to_string_lossy().to_string(),
            created_at: now,
            status: WorkspaceStatus::Active,
            last_heartbeat: now,
        };
        self.db.put_workspace(&ws)?;

        let op = OperationLog {
            id: Uuid::new_v4(),
            operation: "new".into(),
            timestamp: now,
            details: Some(format!("created workspace '{name}'")),
        };
        self.db.put_operation(&op)?;

        Ok(ws)
    }

    /// All known workspaces, oldest first.
    pub fn list(&self) -> Result<Vec<Workspace>> {
        self.db.workspaces()
    }

    /// Resolve a workspace by name.
    pub fn find(&self, name: &str) -> Result<Workspace> {
        let all = self.db.workspaces()?;
        all.into_iter()
            .filter(|w| w.name == name && w.status == WorkspaceStatus::Active)
            .max_by_key(|w| w.created_at)
            .ok_or_else(|| anyhow::anyhow!("workspace '{name}' not found"))
    }

    /// Refresh a workspace's heartbeat by name.
    pub fn touch(&self, name: &str) -> Result<()> {
        let ws = self.find(name)?;
        self.touch_workspace(&ws)
    }

    /// Refresh a workspace's heartbeat (avoids a second DB lookup when the
    /// caller already resolved the record).
    pub fn touch_workspace(&self, ws: &Workspace) -> Result<()> {
        Heartbeat::new(Path::new(&ws.path)).write()
    }

    /// Whether a workspace's heartbeat is still fresh.
    pub fn is_alive(&self, ws: &Workspace) -> bool {
        Heartbeat::new(Path::new(&ws.path)).is_alive()
    }

    /// Load the baseline manifest recorded at creation time.
    pub fn baseline(&self, ws: &Workspace) -> Result<Manifest> {
        let path = Path::new(&ws.path).join(BASELINE_FILE);
        let raw = fs::read(&path)
            .with_context(|| format!("missing baseline manifest for '{}'", ws.name))?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// Directory holding the workspace's pristine base tree (merge base).
    pub fn baseline_dir(&self, ws: &Workspace) -> PathBuf {
        Path::new(&ws.path).join(BASELINE_DIR)
    }

    /// Empty and recreate the staging directory, returning its path.
    pub fn reset_staging(&self) -> Result<PathBuf> {
        if self.staging_dir.exists() {
            fs::remove_dir_all(&self.staging_dir)?;
        }
        fs::create_dir_all(&self.staging_dir)?;
        Ok(self.staging_dir.clone())
    }

    /// Load the current (live) manifest of a workspace's tracked files.
    pub fn workspace_manifest(&self, ws: &Workspace) -> Result<Manifest> {
        snapshot::compute(Path::new(&ws.path))
    }

    /// The project root's current manifest ("theirs").
    pub fn root_manifest(&self) -> Result<Manifest> {
        snapshot::compute(&self.root)
    }

    /// Record an internal checkpoint for the workspace whose directory is `cwd`.
    pub fn commit(&self, cwd: &Path, message: &str) -> Result<Commit> {
        let ws = self.find_by_path(cwd)?;
        let commit = Commit {
            id: Uuid::new_v4(),
            workspace_id: ws.id,
            message: message.to_string(),
            timestamp: Utc::now(),
        };
        self.db.put_commit(&commit)?;
        Ok(commit)
    }

    /// Internal checkpoints across all workspaces, oldest first.
    pub fn commits(&self) -> Result<Vec<Commit>> {
        self.db.commits()
    }

    /// Resolve the workspace that contains `path`, if any.
    pub fn find_by_path(&self, path: &Path) -> Result<Workspace> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let all = self.db.workspaces()?;
        all.into_iter()
            .filter(|w| {
                let ws_path = Path::new(&w.path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&w.path));
                canonical.starts_with(&ws_path)
            })
            .max_by_key(|w| Path::new(&w.path).components().count())
            .ok_or_else(|| anyhow::anyhow!("not inside a tracked workspace"))
    }

    /// Drop a workspace: detach any children, remove the directory and metadata.
    ///
    /// Reflink-based children hold independent references to their data, so
    /// deleting a parent never corrupts a child; children are simply detached
    /// from the dropped parent. All metadata mutations happen in one atomic
    /// transaction.
    pub fn drop(&self, name: &str) -> Result<()> {
        let ws = self.find(name)?;

        // Detach children (CoW refcounts stay valid; lineage just changes).
        let detached: Vec<Workspace> = self
            .db
            .workspaces()?
            .into_iter()
            .filter(|w| w.parent_id == Some(ws.id))
            .map(|mut child| {
                child.parent_id = None;
                child
            })
            .collect();

        let path = PathBuf::from(&ws.path);
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing workspace directory {}", path.display()))?;
        }
        self.db
            .apply_workspace_changes(&[ws.id.to_string()], &detached)?;

        let op = OperationLog {
            id: Uuid::new_v4(),
            operation: "drop".into(),
            timestamp: Utc::now(),
            details: Some(format!("dropped workspace '{name}'")),
        };
        self.db.put_operation(&op)?;
        Ok(())
    }

    /// Append an `undo` marker to the operation log and return it.
    pub fn undo(&self) -> Result<OperationLog> {
        let op = OperationLog {
            id: Uuid::new_v4(),
            operation: "undo".into(),
            timestamp: Utc::now(),
            details: Some("rolled back last metadata transaction".into()),
        };
        self.db.put_operation(&op)?;
        Ok(op)
    }

    /// Reconcile metadata against the physical workspace directory.
    pub fn reconcile(&self) -> Result<crate::engine::reconciler::ReconcileReport> {
        crate::engine::reconciler::reconcile(&self.db, &self.workspaces_dir)
    }

    /// Remove engine-internal sidecar files from a staging tree before merge.
    fn scrub(path: &Path) -> Result<()> {
        for name in RESERVED_FILES {
            let f = path.join(name);
            if f.exists() {
                let _ = fs::remove_file(f);
            }
        }
        Ok(())
    }

    /// Stage a workspace's tracked changes into the staging directory and
    /// return the staging path.
    pub fn stage(&self, ws: &Workspace) -> Result<PathBuf> {
        if self.staging_dir.exists() {
            fs::remove_dir_all(&self.staging_dir)?;
        }
        fs::create_dir_all(&self.staging_dir)?;
        crate::merger::merger::stage_changes(Path::new(&ws.path), &self.staging_dir)?;
        Self::scrub(&self.staging_dir)?;
        Ok(self.staging_dir.clone())
    }

    /// Finalise a successful merge: reclaim the workspace directory but keep
    /// the metadata record, flagged `Merged`, so `shadow status` can keep
    /// reporting the workspace as a consumed member of the project history.
    ///
    /// The record survives reconciliation (which otherwise prunes directories
    /// that vanished) and `find` no longer resolves it, so a merged name is
    /// inert for checkout/exec/merge while still being visible as history.
    pub fn finalize_merged(&self, ws: &Workspace) -> Result<()> {
        let path = PathBuf::from(&ws.path);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        let mut merged = ws.clone();
        merged.status = WorkspaceStatus::Merged;
        self.db.put_workspace(&merged)?;
        Ok(())
    }

    /// The filesystem name for this project's root (cached at open).
    pub fn fs_name(&self) -> &str {
        &self.fs_name
    }
}
