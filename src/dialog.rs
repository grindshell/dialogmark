//! Runtime entry point. Lean, fail-fast parse for gameplay.

use crate::blocks::{DialogBlock, walk_dialog};
use crate::error::DialogError;
use crate::frontmatter::{DialogFrontmatter, parse_frontmatter_failfast};

#[derive(Debug, Clone)]
pub struct Dialog {
    pub frontmatter: DialogFrontmatter,
    pub blocks: Vec<DialogBlock>,
}

impl Dialog {
    /// Parse a dialog markdown source for the runtime. Errors out on the first
    /// frontmatter problem; assumes the editor already validated the file.
    pub fn parse(content: &str) -> Result<Self, DialogError> {
        let (yaml, blocks) = walk_dialog(content);
        let frontmatter = parse_frontmatter_failfast(&yaml)?;
        Ok(Self {
            frontmatter,
            blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockKind;
    use crate::error::{DialogError, FrontmatterError};

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
}
