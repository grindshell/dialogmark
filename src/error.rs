//! Error types for dialog parsing.
//!
//! [`FrontmatterError`] is the leaf diagnostic produced by the YAML-subset
//! walker. [`DialogError`] wraps it (and will grow more variants when the
//! simulation pass lands).

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum FrontmatterError {
    #[error("Required frontmatter key `name` is missing")]
    MissingName,

    #[error("Frontmatter `name` must be alphanumeric ASCII, got {name:?}")]
    InvalidName { name: String },

    #[error("Unknown frontmatter key `{key}`")]
    UnknownKey { key: String },

    #[error("Key `{key}` must be a string, not a YAML list")]
    ListValue { key: String },

    #[error("Quoted frontmatter values are not allowed (key `{key}`)")]
    QuotedValue { key: String },

    #[error("Could not parse frontmatter line: {line:?}")]
    UnparseableLine { line: String },

    #[error("Duplicate `{key}` key")]
    DuplicateKey { key: String },

    #[error("Both `author` and `authors` were given")]
    AuthorAliasConflict,
}

/// A parse-time fault in a **choice set** — the trailing Markdown link-list a
/// section uses to present the player's options
/// (CHOICES_AND_SEGMENTED_WALK.md §2/§6). Surfaced fail-fast as a
/// [`DialogError::ChoiceSet`] on the runtime parse, and (editor feature) with a
/// line for the author.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ChoiceSetError {
    /// A choice set is followed by more content in the same section (before the
    /// next heading / end of input). The blocks after it would be unreachable.
    #[error("a choice set must be the last block in its section (line {line})")]
    NonTrailing { line: usize },

    /// A top-level list looks like a choice set but mixes single-link items with
    /// non-link items. Either every item is a `[label](#Target)` link (a choice
    /// set) or none are (ordinary prose, skipped); a mix is a malformed document.
    #[error("a choice-set list mixes links and non-links (line {line})")]
    Malformed { line: usize },
}

impl ChoiceSetError {
    /// The source line the fault is attributed to (for fail-fast ordering and
    /// the editor's author-facing diagnostics).
    pub fn line(&self) -> usize {
        match self {
            ChoiceSetError::NonTrailing { line } | ChoiceSetError::Malformed { line } => *line,
        }
    }
}

#[derive(Debug, Error)]
pub enum DialogError {
    #[error("frontmatter error: {0}")]
    Frontmatter(#[from] FrontmatterError),

    /// A choice set (trailing link-list) was malformed or non-trailing
    /// (CHOICES_AND_SEGMENTED_WALK.md §6). A parse-time fault: the dialog never
    /// walks.
    #[error("choice set error: {0}")]
    ChoiceSet(#[from] ChoiceSetError),

    /// The Luau VM rejected an attempt to create or seed the persistent
    /// `state` table. Surfaced before any user code has executed; the walk
    /// never starts.
    #[error("failed to initialize dialog state: {0}")]
    StateInit(String),

    /// An mlua call that should not be reachable from user code (e.g. iterating
    /// the state table for a snapshot) failed. Surfaced as an `Err` rather than
    /// a termination because it indicates a VM-level fault, not a script bug.
    #[error("internal Lua error: {0}")]
    LuaInternal(String),
}
