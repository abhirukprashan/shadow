// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Serializable data models persisted in the [`redb`](redb) metadata store.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Lifecycle state of a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceStatus {
    /// Heartbeat is fresh and the workspace is in use.
    Active,
    /// Changes were merged back into the project root.
    Merged,
    /// Explicitly removed via `shadow drop`.
    Dropped,
    /// Heartbeat expired; safe to garbage-collect.
    Abandoned,
}

/// A CoW workspace tracked by the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    /// Parent workspace id for stackable (stacked) workspaces.
    pub parent_id: Option<Uuid>,
    /// Physical directory backing this workspace.
    pub path: String,
    pub created_at: DateTime<Utc>,
    pub status: WorkspaceStatus,
    /// Timestamp of the last observed heartbeat.
    pub last_heartbeat: DateTime<Utc>,
}

impl Workspace {
    /// Human-readable lineage, e.g. `main -> feature-x`.
    pub fn lineage(&self) -> String {
        self.name.clone()
    }
}

/// A lightweight internal checkpoint within a workspace timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

/// An entry in the append-only operation log, used for `shadow undo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationLog {
    pub id: Uuid,
    pub operation: String,
    pub timestamp: DateTime<Utc>,
    pub details: Option<String>,
}
