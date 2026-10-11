// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Publishable status pages: admin management plus the cached, read-only
//! public view (#1412).

pub mod render;
pub mod service;
pub mod snapshot;
pub mod types;

pub use service::{is_well_formed_slug, PublicStatusPageService, SLUG_LEN};
pub use snapshot::{PublishedSnapshot, StatusSnapshotCache, REFRESH_INTERVAL};
pub use types::*;
