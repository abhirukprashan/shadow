// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright (c) 2026 Abhiruk Perera

//! Merge machinery: staging/atomic application, line-level diff3 merging,
//! conflict reporting and workspace→root / workspace→workspace integration.

pub mod conflicts;
pub mod diff3;
pub mod merger;
