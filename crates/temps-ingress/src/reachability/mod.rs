// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Making a project reachable (ADR 064): per address, whether visitors can
//! reach it, why not, and one guided, undoable way to fix it.
//!
//! - [`compute`] judges each address from gathered facts (D3).
//! - [`addresses`] names the primary address, the preview domain's status
//!   and the deploy warning (ADR 068 D2, D4, D5).
//! - [`plan`] picks the ways in the cluster can offer (D4).
//! - [`run`] carries one out step by step, recording what it creates so
//!   undo removes exactly that.
//! - [`token_check`] tells which Cloudflare permissions a token has (D8).
//! - [`service`] gathers the facts and owns runs; [`handlers`] serves them.

pub mod address_handlers;
pub mod addresses;
pub mod compute;
pub mod environment_address;
pub mod handlers;
pub mod model;
pub mod plan;
pub mod run;
pub mod service;
pub mod token_check;

pub use environment_address::EnvironmentAddressService;
pub use service::{ReachabilityDeps, ReachabilityError, ReachabilityService, WaysIn};
