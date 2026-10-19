// SPDX-License-Identifier: GPL-3.0-only
// Copyright (c) 2026 Abhiruk Perera

//! Merge machinery: staging/atomic application, line-level diff3 merging,
//! conflict reporting and workspace→root / workspace→workspace integration.

pub mod conflicts;
pub mod diff3;
#[allow(clippy::module_inception)] // historical layout: merger::merger holds the primitives
pub mod merger;
