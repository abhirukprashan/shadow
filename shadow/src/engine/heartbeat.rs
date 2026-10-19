// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Workspace liveness tracking via timestamp heartbeats.
//!
//! Raw OS PIDs are deliberately avoided (PID-reuse traps). Instead each active
//! workspace owns a `.shadow-lock` file whose fresh timestamp marks it alive.
//! A stale heartbeat means the workspace is abandoned and may be reclaimed.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
    ///
    /// The parent directory is expected to exist (it is created with the
    /// workspace); resurrecting a deleted directory here would defeat the
    /// reconciler, so no `create_dir_all` is performed.
    pub fn write(&self) -> Result<()> {
        let data = HeartbeatData {
            timestamp: Utc::now(),
            token: uuid::Uuid::new_v4().to_string(),
        };
        fs::write(&self.path, serde_json::to_vec(&data)?)?;
        Ok(())
    }

    /// Alias for [`Heartbeat::write`], used by long-running agents.
    pub fn beat(&self) -> Result<()> {
        self.write()
    }

    /// Whether the heartbeat exists and is younger than [`HEARTBEAT_TTL`].
    pub fn is_alive(&self) -> bool {
        self.last_timestamp()
            .map(|ts| {
                Utc::now().signed_duration_since(ts).num_seconds() < HEARTBEAT_TTL.as_secs() as i64
            })
            .unwrap_or(false)
    }

    pub fn last_timestamp(&self) -> Option<DateTime<Utc>> {
        let raw = fs::read(&self.path).ok()?;
        serde_json::from_slice::<HeartbeatData>(&raw)
            .ok()
            .map(|d| d.timestamp)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start a background thread that refreshes this heartbeat every
    /// [`HEARTBEAT_INTERVAL`] until the returned guard is dropped.
    ///
    /// Used around long-running operations (`checkout` subshells, `exec`
    /// commands) so workspaces in active use never look abandoned, no matter
    /// how long the command runs.
    pub fn start_refresher(self) -> HeartbeatGuard {
        let stop = Arc::new(AtomicBool::new(false));
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let pair2 = pair.clone();
        let stop2 = stop.clone();

        // Beat immediately so liveness starts fresh even for short commands.
        let _ = self.write();

        let handle = std::thread::spawn(move || {
            let (lock, cvar) = &*pair2;
            let mut stopped = lock.lock().unwrap();
            while !*stopped {
                let (guard, timeout) = cvar
                    .wait_timeout(stopped, HEARTBEAT_INTERVAL)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                stopped = guard;
                if timeout.timed_out() && !*stopped && !stop2.load(Ordering::Relaxed) {
                    drop(stopped);
                    let _ = self.write();
                    stopped = lock.lock().unwrap();
                }
            }
        });

        HeartbeatGuard {
            stop,
            pair,
            handle: Some(handle),
        }
    }
}

/// Stops a [`Heartbeat::start_refresher`] thread on drop. The drop blocks only
/// long enough for the refresher thread to wake and exit (no full-interval
/// sleep), so it is safe to use with `?`-heavy control flow.
pub struct HeartbeatGuard {
    stop: Arc<AtomicBool>,
    pair: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HeartbeatGuard {
    /// Stop refreshing and wait for the refresher thread to finish.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let (lock, cvar) = &*self.pair;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
            cvar.notify_one();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
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

    #[test]
    fn refresher_keeps_heartbeat_alive_and_stops_promptly() {
        let dir = tempdir().unwrap();
        let hb = Heartbeat::new(dir.path());
        let guard = hb.clone().start_refresher();
        assert!(hb.is_alive());
        // Drop must return quickly (well under HEARTBEAT_INTERVAL).
        let started = std::time::Instant::now();
        guard.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
