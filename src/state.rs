//! Persistent dialog state threaded across blocks during simulation.
//!
//! These types are not used by the parsing pass, but land here now so the
//! simulation pass that follows can plug into a stable module layout.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogNext {
    pub t: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DialogState {
    pub idx: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<DialogNext>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, serde_json::Value>,
}
