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

/// One option surfaced to the caller at a choice point
/// (CHOICES_AND_SEGMENTED_WALK.md §3/§4.3): from a scripted
/// `state.next = { t = "present", options = … }` or derived from a trailing
/// choice set. The caller renders these, then hands the chosen one's `id` (and
/// `target`) back to [`crate::Dialog::resume`]. `note` / `disabled` are opaque
/// presentation hints dialogmark passes through untouched and does **not**
/// enforce — a consumer that gates an option re-checks on selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentedOption {
    /// The token the caller hands back on selection (unique within one present).
    pub id: String,
    /// Player-facing choice text.
    pub label: String,
    /// The heading text to resume the walk at (exact-text, like a `goto`).
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
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
    /// A per-segment **managed** override for whether this frame's `#` heading
    /// renders in the body: a code block sets `state.show_heading = true` (or
    /// `false`) to override the dialog-wide frontmatter `show_headings` default
    /// for the section it runs in. `None` when the script didn't set it (use the
    /// frontmatter default). Like `next` it is runtime-owned — never seeded from a
    /// resumed snapshot's extras — so it resets each segment (hence *per-section*).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_heading: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, serde_json::Value>,
}
