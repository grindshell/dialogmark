//! Runtime entry point. Lean, fail-fast parse for gameplay.

use mlua::{Lua, Table};

use crate::blocks::{DialogBlock, walk_dialog};
use crate::error::DialogError;
use crate::frontmatter::{DialogFrontmatter, parse_frontmatter_failfast};
use crate::state::DialogState;
use crate::walker::DialogWalker;

#[derive(Debug, Clone)]
pub struct Dialog {
    pub frontmatter: DialogFrontmatter,
    pub blocks: Vec<DialogBlock>,
}

impl Dialog {
    /// Parse a dialog markdown source for the runtime. Errors out on the first
    /// frontmatter or choice-set problem; assumes the editor already validated
    /// the file.
    pub fn parse(content: &str) -> Result<Self, DialogError> {
        let (yaml, blocks, choice_errors) = walk_dialog(content);
        let frontmatter = parse_frontmatter_failfast(&yaml)?;
        // Fail fast on the earliest choice-set fault (by source line).
        if let Some(err) = choice_errors.into_iter().min_by_key(|e| e.line()) {
            return Err(err.into());
        }
        Ok(Self {
            frontmatter,
            blocks,
        })
    }

    /// Start a fresh walk against the caller-supplied Luau VM, running each
    /// block in a per-walk environment chained to `base_env` (stdlib + the
    /// caller's host modules — CHOICES_AND_SEGMENTED_WALK.md §4.1). The prelude
    /// runs lazily on the first advance call.
    pub fn walk<'a>(
        &'a self,
        lua: &Lua,
        base_env: Table,
        start_idx: usize,
    ) -> Result<DialogWalker<'a>, DialogError> {
        DialogWalker::new(&self.blocks, lua, base_env, start_idx)
    }

    /// Resume a segmented walk at the chosen option's target heading
    /// (CHOICES_AND_SEGMENTED_WALK.md §4.4). Re-runs the prelude into a fresh
    /// per-walk env, restores the saved `extras`, sets `state.choice = choice_id`,
    /// and positions the cursor at `resume_heading` for the next
    /// [`DialogWalker::advance_segment`].
    pub fn resume<'a>(
        &'a self,
        lua: &Lua,
        base_env: Table,
        state: DialogState,
        resume_heading: &str,
        choice_id: Option<String>,
    ) -> Result<DialogWalker<'a>, DialogError> {
        DialogWalker::resume(
            &self.blocks,
            lua,
            base_env,
            state,
            resume_heading,
            choice_id,
        )
    }

    /// The block index of the heading whose text is exactly `name`, or `None`.
    /// Lets a caller that prefers indices map an option's `target` itself.
    pub fn heading_index(&self, name: &str) -> Option<usize> {
        crate::exec::resolve_goto(&self.blocks, name).map(|b| b.idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockKind;
    use crate::error::{ChoiceSetError, DialogError, FrontmatterError};

    #[test]
    fn empty_input_misses_name() {
        let err = Dialog::parse("").unwrap_err();
        assert!(matches!(
            err,
            DialogError::Frontmatter(FrontmatterError::MissingName)
        ));
    }

    #[test]
    fn body_only_no_frontmatter_misses_name() {
        let err = Dialog::parse("# Hello\n\nworld\n").unwrap_err();
        assert!(matches!(
            err,
            DialogError::Frontmatter(FrontmatterError::MissingName)
        ));
    }

    #[test]
    fn frontmatter_only_parses() {
        let d = Dialog::parse("---\nname: Greet\ndescription: hi\n---\n").unwrap();
        assert_eq!(d.frontmatter.name, "Greet");
        assert_eq!(d.frontmatter.description.as_deref(), Some("hi"));
        assert!(d.blocks.is_empty());
    }

    #[test]
    fn frontmatter_and_body_parses() {
        let d = Dialog::parse("---\nname: Greet\n---\n\n# Hi\n\nbody\n").unwrap();
        assert_eq!(d.frontmatter.name, "Greet");
        assert_eq!(d.blocks.len(), 2);
        assert_eq!(d.blocks[0].kind, BlockKind::Heading);
        assert_eq!(d.blocks[0].text, "Hi");
        assert_eq!(d.blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(d.blocks[1].text, "body");
    }

    #[test]
    fn invalid_name_fails() {
        let err = Dialog::parse("---\nname: has space\n---\n").unwrap_err();
        assert!(matches!(
            err,
            DialogError::Frontmatter(FrontmatterError::InvalidName { .. })
        ));
    }

    #[test]
    fn unknown_key_fails() {
        let err = Dialog::parse("---\nname: A\nfoo: bar\n---\n").unwrap_err();
        assert!(matches!(
            err,
            DialogError::Frontmatter(FrontmatterError::UnknownKey { .. })
        ));
    }

    #[test]
    fn parse_rejects_non_trailing_choice_set() {
        let err = Dialog::parse("---\nname: T\n---\n\n# S\n\n- [Go](#Go)\n\nmore\n\n# Go\n\nx\n")
            .unwrap_err();
        assert!(matches!(
            err,
            DialogError::ChoiceSet(ChoiceSetError::NonTrailing { .. })
        ));
    }

    #[test]
    fn parse_accepts_a_valid_choice_set() {
        let d = Dialog::parse("---\nname: T\n---\n\n# S\n\nhi\n\n- [Go](#Go)\n\n# Go\n\nx\n")
            .expect("a valid choice set parses");
        assert!(d.blocks.iter().any(|b| b.kind == BlockKind::Choices));
    }
}
