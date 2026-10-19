// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Workspace lifecycle orchestration: creation, listing, diffing, merging,
//! heartbeat liveness and metadata reconciliation.

pub mod heartbeat;
pub mod reconciler;
pub mod snapshot;
pub mod workspace;
