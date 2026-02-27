//! Minimal discovery stubs.
//!
//! This crate split intentionally keeps the ReAct runner generic. Some provider APIs
//! still accept `Metadata` for back-compat; it is currently an empty placeholder.

use serde::{Deserialize, Serialize};

pub mod stats;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metadata {}
