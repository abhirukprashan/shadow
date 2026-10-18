//! Workspace liveness tracking via timestamp heartbeats.
//!
//! Raw OS PIDs are deliberately avoided (PID-reuse traps). Instead each active
//! workspace owns a `.shadow-lock` file whose fresh timestamp marks it alive.
//! A stale heartbeat means the workspace is abandoned and may be reclaimed.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::fs;
use std::path::Path;
use std::time::Duration;

/// How often an owner is expected to refresh its heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// A heartbeat older than this marks the workspace as abandoned.
pub const HEARTBEAT_TTL: Duration = Duration::from_secs(90);

/// Name of the per-workspace liveness file.
pub const LOCK_FILE: &str = ".shadow-lock";

/// Reader/writer for a single workspace's heartbeat file.
#[derive(Clone, Debug)]
pub struct Heartbeat {
    path: std::path::PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HeartbeatData {
    timestamp: DateTime<Utc>,
    token: String,
}

impl Heartbeat {
    pub fn new(workspace: &Path) -> Self {
        Self {
            path: workspace.join(LOCK_FILE),
        }
    }

    /// Refresh the heartbeat for this workspace.
    pub fn write(&self) -> Result<()> {
        let data = HeartbeatData {
            timestamp: Utc::now(),
            token: uuid::Uuid::new_v4().to_string(),
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, serde_json::to_vec(&data)?)?;
        Ok(())
    }

    /// Alias for [`Heartbeat::write`], used by long-running agents.
    pub fn beat(&self) -> Result<()> {
        self.write()
    }

    /// The interval at which callers should refresh the heartbeat.
    pub fn interval() -> Duration {
        HEARTBEAT_INTERVAL
    }

    /// Whether the heartbeat exists and is younger than [`HEARTBEAT_TTL`].
    pub fn is_alive(&self) -> bool {
        self.last_timestamp()
            .map(|ts| {
                Utc::now().signed_duration_since(ts).num_seconds() < HEARTBEAT_TTL.as_secs() as i64
            })
            .unwrap_or(false)
    }

    /// Last recorded heartbeat timestamp, if any.
    pub fn last_timestamp(&self) -> Option<DateTime<Utc>> {
        let raw = fs::read(&self.path).ok()?;
        serde_json::from_slice::<HeartbeatData>(&raw)
            .ok()
            .map(|d| d.timestamp)
    }

    /// Path to the underlying lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fresh_heartbeat_is_alive() {
        let dir = tempdir().unwrap();
        let hb = Heartbeat::new(dir.path());
        hb.write().unwrap();
        assert!(hb.is_alive());
        assert!(hb.last_timestamp().is_some());
    }

    #[test]
    fn missing_heartbeat_is_dead() {
        let dir = tempdir().unwrap();
        let hb = Heartbeat::new(dir.path());
        assert!(!hb.is_alive());
    }
}
