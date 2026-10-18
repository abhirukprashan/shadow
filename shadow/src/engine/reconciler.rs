// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Failsafe state reconciliation between the metadata store and the physical
//! workspace directory.
//!
//! If a user (or a rogue script) deletes a workspace directory with `rm -rf`,
//! the `redb` store is left holding "ghost" metadata. Rather than panicking,
//! reconciliation prunes records whose backing directory no longer exists and
//! reports any orphans it repaired.

use crate::storage::db::Db;
use anyhow::Result;
use std::path::Path;

/// Summary of a reconciliation pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileReport {
    /// Number of ghost metadata rows removed.
    pub pruned: usize,
    /// Number of workspace records inspected.
    pub inspected: usize,
}

/// Cross-reference the metadata store against the physical workspace folders,
/// pruning records whose directory no longer exists (or dropping records whose
/// heartbeat has long expired) instead of panicking.
pub fn reconcile(db: &Db, workspaces_dir: &Path) -> Result<ReconcileReport> {
    std::fs::create_dir_all(workspaces_dir)?;

    let mut report = ReconcileReport::default();
    let known = db.workspaces()?;

    for ws in known {
        report.inspected += 1;
        let path = Path::new(&ws.path);
        let alive_on_disk = path.exists();
        if !alive_on_disk {
            db.remove_workspace(&ws.id.to_string())?;
            report.pruned += 1;
        }
    }
    Ok(report)
}
