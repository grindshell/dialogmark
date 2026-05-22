//! Block walker for dialog markdown.
//!
//! The dialog runtime considers exactly three Markdown structures as "blocks":
//! headings, paragraphs, and fenced code blocks. Lists, blockquotes, tables,
//! indented code, and the YAML metadata block are skipped — they do not
//! advance the `idx` cursor.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Heading,
    Paragraph,
    Code,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogBlock {
    /// 0-based index into the heading/paragraph/code-block stream.
    pub idx: usize,
    pub kind: BlockKind,
    /// For Heading: the heading text (plain, trimmed).
    /// For Code: the raw fenced source.
    /// For Paragraph: the concatenated text content (trimmed).
    pub text: String,
    /// 1-based line in the original markdown of the block's first content
    /// line. For a fenced code block this is the line of the first source
    /// line, not the opening fence.
    pub start_line: usize,
}

fn is_container_start(tag: &Tag) -> bool {
    matches!(
        tag,
        Tag::List(_)
            | Tag::Item
            | Tag::BlockQuote(_)
            | Tag::FootnoteDefinition(_)
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
    )
}

fn is_container_end(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::BlockQuote(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
    )
}

fn compute_line(content: &str, byte_offset: usize) -> usize {
    let offset = byte_offset.min(content.len());
    content[..offset].matches('\n').count() + 1
}

enum ActiveBlock {
    Idle,
    In {
        kind: BlockKind,
        start_offset: usize,
        text: String,
        first_text_offset: Option<usize>,
    },
}

/// Walk the dialog's markdown event stream once and return the top-level
/// heading/paragraph/fenced-code blocks in order.
pub fn extract_blocks(content: &str) -> Vec<DialogBlock> {
    walk_dialog(content).1
}

/// Single-pass walk that lifts both the YAML metadata block's raw text and
/// the top-level heading/paragraph/fenced-code blocks out of `content`.
///
/// Used by [`crate::Dialog::parse`] so the runtime parses the dialog markdown
/// exactly once. The standalone [`extract_blocks`] is a thin wrapper that
/// discards the YAML half for callers that only want the blocks.
pub(crate) fn walk_dialog(content: &str) -> (String, Vec<DialogBlock>) {
    let parser = Parser::new_ext(content, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    let mut yaml = String::new();
    let mut blocks = Vec::new();
    let mut idx: usize = 0;
    // Depth inside containers (lists, blockquotes, tables). Blocks are only
    // emitted when this is zero AND we're not inside the metadata block.
    let mut nest: u32 = 0;
    let mut in_metadata = false;
    let mut active = ActiveBlock::Idle;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::MetadataBlock(_)) => {
                in_metadata = true;
            }
            Event::End(TagEnd::MetadataBlock(_)) => {
                in_metadata = false;
            }
            Event::Start(ref tag) if is_container_start(tag) => {
                nest += 1;
            }
            Event::End(ref tag) if is_container_end(tag) => {
                nest = nest.saturating_sub(1);
            }
            Event::Start(Tag::Heading { .. }) if !in_metadata && nest == 0 => {
                active = ActiveBlock::In {
                    kind: BlockKind::Heading,
                    start_offset: range.start,
                    text: String::new(),
                    first_text_offset: None,
                };
            }
            Event::Start(Tag::Paragraph) if !in_metadata && nest == 0 => {
                active = ActiveBlock::In {
                    kind: BlockKind::Paragraph,
                    start_offset: range.start,
                    text: String::new(),
                    first_text_offset: None,
                };
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) if !in_metadata && nest == 0 => {
                active = ActiveBlock::In {
                    kind: BlockKind::Code,
                    start_offset: range.start,
                    text: String::new(),
                    first_text_offset: None,
                };
            }
            Event::Text(t) => {
                if in_metadata {
                    yaml.push_str(&t);
                } else if let ActiveBlock::In {
                    text,
                    first_text_offset,
                    ..
                } = &mut active
                {
                    text.push_str(&t);
                    if first_text_offset.is_none() {
                        *first_text_offset = Some(range.start);
                    }
                }
            }
            Event::Code(t) => {
                if let ActiveBlock::In {
                    kind,
                    text,
                    first_text_offset,
                    ..
                } = &mut active
                {
                    // Inline code in headings/paragraphs gets folded into the
                    // plain text. Fenced code blocks emit Text, not Code.
                    if *kind != BlockKind::Code {
                        text.push_str(&t);
                        if first_text_offset.is_none() {
                            *first_text_offset = Some(range.start);
                        }
                    }
                }
            }
            Event::SoftBreak => {
                if let ActiveBlock::In { kind, text, .. } = &mut active
                    && *kind != BlockKind::Code
                {
                    text.push(' ');
                }
            }
            Event::HardBreak => {
                if let ActiveBlock::In { kind, text, .. } = &mut active
                    && *kind != BlockKind::Code
                {
                    text.push('\n');
                }
            }
            Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::CodeBlock) => {
                if let ActiveBlock::In {
                    kind,
                    start_offset,
                    text,
                    first_text_offset,
                } = std::mem::replace(&mut active, ActiveBlock::Idle)
                {
                    let line = compute_line(content, first_text_offset.unwrap_or(start_offset));
                    let cleaned = match kind {
                        BlockKind::Code => text,
                        _ => text.trim().to_string(),
                    };
                    blocks.push(DialogBlock {
                        idx,
                        kind,
                        text: cleaned,
                        start_line: line,
                    });
                    idx += 1;
                }
            }
            _ => {}
        }
    }

    (yaml, blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(blocks: &[DialogBlock]) -> Vec<BlockKind> {
        blocks.iter().map(|b| b.kind).collect()
    }

    #[test]
    fn counts_only_heading_paragraph_code() {
        let src = "# Hi\n\nfirst paragraph\n\n- list item one\n- list item two\n\n> a blockquote line\n\n```luau\nlocal x = 1\n```\n";
        let blocks = extract_blocks(src);
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Code],
        );
        assert_eq!(blocks[0].text, "Hi");
        assert_eq!(blocks[1].text, "first paragraph");
        assert!(blocks[2].text.contains("local x = 1"));
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.idx, i);
        }
    }

    #[test]
    fn skips_frontmatter_and_indented_code() {
        let src = "---\nname: A\n---\n\n# Title\n\n    not a fenced block\n\nbody\n";
        let blocks = extract_blocks(src);
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Heading, BlockKind::Paragraph]
        );
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[1].text, "body");
    }

    #[test]
    fn start_line_is_one_based() {
        let src = "# Hi\n\nfirst paragraph\n";
        let blocks = extract_blocks(src);
        assert_eq!(blocks[0].start_line, 1);
        assert_eq!(blocks[1].start_line, 3);
    }

    #[test]
    fn fenced_code_preserves_source() {
        let src = "# A\n\n```luau\nlocal x = 1\nlocal y = 2\n```\n";
        let blocks = extract_blocks(src);
        assert_eq!(blocks[1].kind, BlockKind::Code);
        assert_eq!(blocks[1].text, "local x = 1\nlocal y = 2\n");
    }

    #[test]
    fn inline_code_folds_into_heading_text() {
        let src = "# Hello `world`\n";
        let blocks = extract_blocks(src);
        assert_eq!(blocks[0].kind, BlockKind::Heading);
        assert_eq!(blocks[0].text, "Hello world");
    }
}
