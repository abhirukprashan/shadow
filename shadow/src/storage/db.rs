//! Embedded `redb` metadata store.
//!
//! All durable shadow state (workspaces, internal commits and the operation
//! log) lives in a single `.shadow/metadata.redb` file. Concurrent writers
//! (e.g. a swarm of agents) are tolerated through a small retry/backoff loop.

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

    /// Run `f` against the database, retrying with exponential backoff when the
    /// write lock is contended by another process/agent.
    pub fn with_retry<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&Database) -> Result<T>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let guard = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("metadata database mutex poisoned"))?;
            match f(&guard) {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if attempt >= MAX_RETRIES {
                        return Err(err);
                    }
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(5 * attempt as u64));
                }
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

    /// Fetch all workspace records.
    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.with_retry(|db| {
            let txn = db.begin_read()?;
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
        })
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

    /// All internal checkpoints, oldest first.
    pub fn commits(&self) -> Result<Vec<Commit>> {
        self.with_retry(|db| {
            let txn = db.begin_read()?;
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
        })
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

    /// All operations, oldest first.
    pub fn operations(&self) -> Result<Vec<OperationLog>> {
        self.with_retry(|db| {
            let txn = db.begin_read()?;
            let table = txn.open_table(OPS)?;
            let mut out = Vec::new();
            for row in table.iter()? {
                let (_, value) = row?;
                if let Ok(op) = serde_json::from_slice::<OperationLog>(value.value()) {
                    out.push(op);
                }
            }
            out.sort_by_key(|o| o.timestamp);
            Ok(out)
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
}
