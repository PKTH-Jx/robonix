// SPDX-License-Identifier: MulanPSL-2.0

//! SRDL parsing and interpretation for capability-call decisions.

use serde::{Deserialize, Serialize};

pub mod interpreter;
pub mod srdl;

/// Capability-call decision returned by SRDL enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}
