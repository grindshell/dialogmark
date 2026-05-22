//! Dialogmark — Markdown + Luau dialog parsing for Grindshell.
//!
//! This crate parses `.md` dialog files (YAML-style frontmatter + body) into
//! a structured `Dialog`, and (in a follow-up pass) walks them against a
//! caller-supplied Luau VM.
//!
//! Two surfaces, sharing one parser:
//!
//! - **Runtime** (default features) — [`Dialog::parse`] returns
//!   `Result<Dialog, DialogError>`, failing fast on the first error.
//! - **Editor** (behind the `editor` Cargo feature) — [`parse_dialog`]
//!   collects every error, renders HTML, and surfaces both for the editor's
//!   Validate button.

mod blocks;
mod dialog;
mod error;
mod frontmatter;
mod state;

pub use blocks::{BlockKind, DialogBlock, extract_blocks};
pub use dialog::Dialog;
pub use error::{DialogError, FrontmatterError};
pub use frontmatter::DialogFrontmatter;
pub use state::{DialogNext, DialogState};

#[cfg(feature = "editor")]
mod editor;
#[cfg(feature = "editor")]
pub use editor::{ParsedDialog, parse_dialog};
#[cfg(feature = "editor")]
pub use frontmatter::parse_frontmatter_lenient;
