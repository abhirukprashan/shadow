// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Embedded `redb` metadata store.
//!
//! All durable shadow state (workspaces, internal commits and the operation
//! log) lives in a single `.shadow/metadata.redb` file. Retries are reserved
//! for genuine transient storage conditions (interrupted/would-block I/O);
//! logic errors surface immediately instead of being masked by backoff.

// `redb::Error` is a large flat enum; boxing it would churn every call site
// for a negligible gain on the error path only.
#![allow(clippy::result_large_err)]

use crate::storage::models::{Commit, OperationLog, Workspace};
use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WORKSPACES: TableDefinition<&str, &[u8]> = TableDefinition::new("workspaces");
const COMMITS: TableDefinition<&str, &[u8]> = TableDefinition::new("commits");
const OPS: TableDefinition<&str, &[u8]> = TableDefinition::new("operations");

const MAX_RETRIES: u32 = 8;

/// Handle to the embedded metadata database. Cheap to clone; the underlying
/// [`redb::Database`] is shared behind an `Arc<Mutex<..>>`.
#[derive(Clone)]
pub struct Db {
    db: Arc<Mutex<Database>>,
}

/// Whether a redb error is worth retrying (transient I/O only).
///
/// Contention inside this process is already serialised by the mutex, so the
/// only realistic transient failures are interrupted/would-block I/O from the
/// storage layer. Serde failures, corruption, type mismatches, etc. are
/// deterministic and retried never. (`redb::Error` is a flat enum that
/// collapses nested storage/table/transaction errors into top-level variants.)
fn is_transient(err: &redb::Error) -> bool {
    match err {
        redb::Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ),
        _ => false,
    }
}

impl Db {
    /// Open (creating if necessary) the metadata database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        {
            let txn = db.begin_write()?;
            let _ = txn.open_table(WORKSPACES)?;
            let _ = txn.open_table(COMMITS)?;
            let _ = txn.open_table(OPS)?;
            txn.commit()?;
        }
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Database>> {
        self.db
            .lock()
            .map_err(|_| anyhow::anyhow!("metadata database mutex poisoned"))
    }

    /// Run a write `f` against the database, retrying with exponential backoff
    /// only when the storage layer reports a transient I/O condition.
    pub fn with_retry<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&Database) -> std::result::Result<T, redb::Error>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let guard = self.lock()?;
            match f(&guard) {
                Ok(value) => return Ok(value),
                Err(err) if attempt < MAX_RETRIES && is_transient(&err) => {
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(5 * attempt as u64));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Insert or update a workspace record.
    pub fn put_workspace(&self, ws: &Workspace) -> Result<()> {
        let key = ws.id.to_string();
        let value = serde_json::to_vec(ws)?;
        self.with_retry(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(WORKSPACES)?;
                table.insert(key.as_str(), value.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// Apply several workspace insertions and removals in a single atomic
    /// transaction (one fsync instead of one per record).
    pub fn apply_workspace_changes(&self, removes: &[String], puts: &[Workspace]) -> Result<()> {
        let puts: Vec<(String, Vec<u8>)> = puts
            .iter()
            .map(|w| Ok((w.id.to_string(), serde_json::to_vec(w)?)))
            .collect::<Result<_>>()?;
        self.with_retry(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(WORKSPACES)?;
                for (key, value) in &puts {
                    table.insert(key.as_str(), value.as_slice())?;
                }
                for key in removes {
                    table.remove(key.as_str())?;
                }
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// Remove a workspace record (used when pruning ghost metadata).
    pub fn remove_workspace(&self, key: &str) -> Result<()> {
        self.with_retry(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(WORKSPACES)?;
                table.remove(key)?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// Fetch all workspace records. Reads take the lock once and never retry.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let guard = self.lock()?;
        let txn = guard.begin_read()?;
        let table = txn.open_table(WORKSPACES)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            if let Ok(ws) = serde_json::from_slice::<Workspace>(value.value()) {
                out.push(ws);
            }
        }
        out.sort_by_key(|w| w.created_at);
        Ok(out)
    }

    /// Record a lightweight internal commit/checkpoint.
    pub fn put_commit(&self, commit: &Commit) -> Result<()> {
        let key = commit.id.to_string();
        let value = serde_json::to_vec(commit)?;
        self.with_retry(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(COMMITS)?;
                table.insert(key.as_str(), value.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// All internal checkpoints, oldest first. Reads never retry.
    pub fn commits(&self) -> Result<Vec<Commit>> {
        let guard = self.lock()?;
        let txn = guard.begin_read()?;
        let table = txn.open_table(COMMITS)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            if let Ok(c) = serde_json::from_slice::<Commit>(value.value()) {
                out.push(c);
            }
        }
        out.sort_by_key(|c| c.timestamp);
        Ok(out)
    }

    /// Append an operation to the immutable operation log.
    pub fn put_operation(&self, op: &OperationLog) -> Result<()> {
        let key = op.id.to_string();
        let value = serde_json::to_vec(op)?;
        self.with_retry(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(OPS)?;
                table.insert(key.as_str(), value.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::WorkspaceStatus;
    use tempfile::tempdir;

    fn sample_workspace() -> Workspace {
        Workspace {
            id: uuid::Uuid::new_v4(),
            name: "dev".into(),
            parent_id: None,
            path: "/tmp/ws".into(),
            created_at: chrono::Utc::now(),
            status: WorkspaceStatus::Active,
            last_heartbeat: chrono::Utc::now(),
        }
    }

    #[test]
    fn round_trips_workspace() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("meta.redb")).unwrap();
        let ws = sample_workspace();
        db.put_workspace(&ws).unwrap();
        let all = db.workspaces().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, ws.id);
    }

    #[test]
    fn removes_workspace() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("meta.redb")).unwrap();
        let ws = sample_workspace();
        db.put_workspace(&ws).unwrap();
        db.remove_workspace(&ws.id.to_string()).unwrap();
        assert!(db.workspaces().unwrap().is_empty());
    }

    #[test]
    fn batch_applies_removes_and_puts_atomically() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("meta.redb")).unwrap();
        let old = sample_workspace();
        let fresh = sample_workspace();
        db.put_workspace(&old).unwrap();

        db.apply_workspace_changes(&[old.id.to_string()], std::slice::from_ref(&fresh))
            .unwrap();

        let all = db.workspaces().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, fresh.id);
    }

    #[test]
    fn non_transient_errors_fail_fast() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("meta.redb")).unwrap();
        // A logic error inside with_retry must not be retried with backoff:
        // inject a closure that always fails deterministically and assert the
        // error surfaces immediately (well under the ~180ms total backoff).
        let started = std::time::Instant::now();
        let result: Result<()> = db.with_retry(|_| Err(redb::Error::Corrupted("boom".into())));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
