// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Shadow - a high-performance, OS-native copy-on-write (CoW) workspace
//! orchestration engine built on filesystem reflinks.
//!
//! The crate is split into focused modules:
//! - [`co_w`]: low-level copy-on-write reflink primitives and filesystem probes.
//! - [`engine`]: workspace lifecycle, heartbeat liveness and reconciliation.
//! - [`storage`]: the embedded `redb` metadata store and its data models.
//! - [`merger`]: staging and atomic directory-swap merge machinery.
//! - [`utils`]: shared filesystem helpers.
//! - [`cli`]: command-line surface built with `clap`.

pub mod cli;
pub mod co_w;
pub mod engine;
pub mod merger;
pub mod storage;
pub mod utils;
