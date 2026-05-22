//! Error types for dialog parsing.
//!
//! [`FrontmatterError`] is the leaf diagnostic produced by the YAML-subset
//! walker. [`DialogError`] wraps it (and will grow more variants when the
//! simulation pass lands).

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
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

#[derive(Debug, Error)]
pub enum DialogError {
    #[error("frontmatter error: {0}")]
    Frontmatter(#[from] FrontmatterError),
}
