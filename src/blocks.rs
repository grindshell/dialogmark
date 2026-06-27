//! Block walker for dialog markdown.
//!
//! The dialog runtime considers exactly three Markdown structures as "blocks":
//! headings, paragraphs, and fenced code blocks. Lists, blockquotes, tables,
//! indented code, and the YAML metadata block are skipped — they do not
//! advance the `idx` cursor.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

use crate::error::ChoiceSetError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Heading,
    Paragraph,
    Code,
    /// A **choice set** (CHOICES_AND_SEGMENTED_WALK.md §2): a trailing top-level
    /// link-list presenting the player's options. Carries its parsed items in
    /// [`DialogBlock::choices`]; reaching it pauses the walk.
    Choices,
}

/// One option of a [`BlockKind::Choices`] block: the player-facing `label` and
/// the heading `target` selecting it resumes the walk at (the `#Target`
/// fragment, exact-text — same rule as a `goto`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceItem {
    pub label: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogBlock {
    /// 0-based index into the heading/paragraph/code-block/choices stream.
    pub idx: usize,
    pub kind: BlockKind,
    /// For Heading: the heading text (plain, trimmed).
    /// For Code: the raw fenced source.
    /// For Paragraph: the concatenated text content (trimmed).
    /// For Choices: the items' labels joined by `" / "` (a human-readable
    /// preview; the structured items live in [`Self::choices`]).
    pub text: String,
    /// 1-based line in the original markdown of the block's first content
    /// line. For a fenced code block this is the line of the first source
    /// line, not the opening fence.
    pub start_line: usize,
    /// The parsed options for a [`BlockKind::Choices`] block; `None` for every
    /// other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<ChoiceItem>>,
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
pub(crate) fn walk_dialog(content: &str) -> (String, Vec<DialogBlock>, Vec<ChoiceSetError>) {
    let parser = Parser::new_ext(content, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    let mut yaml = String::new();
    let mut blocks = Vec::new();
    let mut idx: usize = 0;
    // Depth inside containers (lists, blockquotes, tables). Blocks are only
    // emitted when this is zero AND we're not inside the metadata block.
    let mut nest: u32 = 0;
    let mut in_metadata = false;
    let mut active = ActiveBlock::Idle;
    // Choice-set capture (CHOICES_AND_SEGMENTED_WALK.md §2): a top-level list is
    // taken as a candidate choice set and re-parsed from its source span once it
    // closes. While `list_capture` is open, inner events are ignored (the line
    // parser owns the structure); only nested list start/end adjust `list_depth`
    // so we know when the outer list closes.
    let mut choice_errors: Vec<ChoiceSetError> = Vec::new();
    let mut list_capture: Option<std::ops::Range<usize>> = None;
    let mut list_depth: u32 = 0;

    for (event, range) in parser.into_offset_iter() {
        if list_capture.is_some() {
            match event {
                Event::Start(Tag::List(_)) => list_depth += 1,
                Event::End(TagEnd::List(_)) => {
                    list_depth -= 1;
                    if list_depth == 0 {
                        let span = list_capture.take().expect("capture is open");
                        finalize_choice_list(
                            content,
                            span,
                            &mut idx,
                            &mut blocks,
                            &mut choice_errors,
                        );
                    }
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::MetadataBlock(_)) => {
                in_metadata = true;
            }
            Event::End(TagEnd::MetadataBlock(_)) => {
                in_metadata = false;
            }
            // A top-level list (outside metadata, not mid heading/paragraph/
            // code) is a candidate choice set: capture its source span and let
            // the inner-event branch above own it until it closes.
            Event::Start(Tag::List(_))
                if !in_metadata && nest == 0 && matches!(active, ActiveBlock::Idle) =>
            {
                list_capture = Some(range.clone());
                list_depth = 1;
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
                        choices: None,
                    });
                    idx += 1;
                }
            }
            _ => {}
        }
    }

    validate_choice_trailing(&blocks, &mut choice_errors);
    (yaml, blocks, choice_errors)
}

// --- choice-set parsing (CHOICES_AND_SEGMENTED_WALK.md §2/§6) ----------------

/// Parse one captured list line as a choice-set item `- [label](#target)`.
/// Returns `None` when the line isn't a single-link list item — letting the
/// caller tell an all-links list (a choice set) from prose.
fn parse_choice_line(line: &str) -> Option<ChoiceItem> {
    let marker = line.chars().next()?;
    if !matches!(marker, '-' | '*' | '+') {
        return None;
    }
    let after_marker = &line[marker.len_utf8()..];
    if !after_marker.starts_with(char::is_whitespace) {
        return None;
    }
    // The remainder must be exactly `[label](#target)`.
    let body = after_marker.trim_start().strip_prefix('[')?;
    let close = body.find("](")?;
    let label = body[..close].trim();
    let dest = body[close + 2..].strip_prefix('#')?;
    let target = dest.strip_suffix(')')?.trim();
    if label.is_empty() || target.is_empty() {
        return None;
    }
    Some(ChoiceItem {
        label: label.to_string(),
        target: target.to_string(),
    })
}

/// Re-parse a captured top-level list's source span as a candidate choice set.
/// Emits a [`BlockKind::Choices`] block when every non-blank line is a single
/// link; records a [`ChoiceSetError::Malformed`] when it mixes links and
/// non-links; and silently skips a list with no links (ordinary prose, as the
/// walker has always done).
fn finalize_choice_list(
    content: &str,
    span: std::ops::Range<usize>,
    idx: &mut usize,
    blocks: &mut Vec<DialogBlock>,
    errors: &mut Vec<ChoiceSetError>,
) {
    let start = span.start.min(content.len());
    let end = span.end.min(content.len());
    let line = compute_line(content, start);

    let mut items: Vec<ChoiceItem> = Vec::new();
    let mut nonlink = 0usize;
    for raw in content[start..end].lines() {
        let l = raw.trim();
        if l.is_empty() {
            continue;
        }
        match parse_choice_line(l) {
            Some(item) => items.push(item),
            None => nonlink += 1,
        }
    }

    if items.is_empty() {
        // No link items — an ordinary prose list. Skip, as before.
        return;
    }
    if nonlink > 0 {
        // Looks like a choice set but mixes links and non-links.
        errors.push(ChoiceSetError::Malformed { line });
        return;
    }
    let label = items
        .iter()
        .map(|c| c.label.as_str())
        .collect::<Vec<_>>()
        .join(" / ");
    blocks.push(DialogBlock {
        idx: *idx,
        kind: BlockKind::Choices,
        text: label,
        start_line: line,
        choices: Some(items),
    });
    *idx += 1;
}

/// Enforce the trailing rule (CHOICES_AND_SEGMENTED_WALK.md §2): a choice set
/// must be the last block of its section — the next block (if any) must be a
/// heading. Anything else in the same section would be unreachable.
fn validate_choice_trailing(blocks: &[DialogBlock], errors: &mut Vec<ChoiceSetError>) {
    for (i, b) in blocks.iter().enumerate() {
        if b.kind != BlockKind::Choices {
            continue;
        }
        match blocks.get(i + 1) {
            None => {}                                          // last block — trailing
            Some(next) if next.kind == BlockKind::Heading => {} // section boundary
            Some(_) => errors.push(ChoiceSetError::NonTrailing { line: b.start_line }),
        }
    }
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

    // --- choice sets (CHOICES_AND_SEGMENTED_WALK.md §2/§6) -------------------

    use crate::error::ChoiceSetError;

    #[test]
    fn choice_set_parses_links_and_preserves_target_spaces() {
        let src = "# Greet\n\nhi\n\n- [Who?](#Who)\n- [Learn](#Learn an art)\n\n# Who\n\nx\n";
        let (_y, blocks, errs) = walk_dialog(src);
        assert!(errs.is_empty(), "valid choice set has no errors: {errs:?}");
        let ch = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Choices)
            .expect("a choices block");
        let items = ch.choices.as_ref().expect("parsed items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Who?");
        assert_eq!(items[0].target, "Who");
        // The `#Target` fragment keeps internal spaces (exact-text goto).
        assert_eq!(items[1].label, "Learn");
        assert_eq!(items[1].target, "Learn an art");
        // The block's `text` preview is the labels joined.
        assert_eq!(ch.text, "Who? / Learn");
    }

    #[test]
    fn choice_set_indices_are_contiguous() {
        let src = "# S\n\nhi\n\n- [Go](#Go)\n\n# Go\n\nx\n";
        let blocks = extract_blocks(src);
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.idx, i, "block {b:?} has idx {}", b.idx);
        }
    }

    #[test]
    fn prose_list_is_skipped_not_a_choice_set() {
        let src = "# S\n\n- just a note\n- another note\n\nbody\n";
        let (_y, blocks, errs) = walk_dialog(src);
        assert!(errs.is_empty(), "a prose list is skipped, not an error");
        assert!(
            blocks.iter().all(|b| b.kind != BlockKind::Choices),
            "a no-links list is not a choice set"
        );
    }

    #[test]
    fn mixed_link_list_is_malformed() {
        let src = "# S\n\n- [Go](#Go)\n- not a link\n\n# Go\n\nx\n";
        let (_y, _b, errs) = walk_dialog(src);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ChoiceSetError::Malformed { .. })),
            "an all-but-one-links list is malformed: {errs:?}"
        );
    }

    #[test]
    fn non_trailing_choice_set_is_rejected() {
        let src = "# S\n\n- [Go](#Go)\n\nmore prose after the choices\n\n# Go\n\nx\n";
        let (_y, _b, errs) = walk_dialog(src);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ChoiceSetError::NonTrailing { .. })),
            "content after a choice set in the same section is non-trailing: {errs:?}"
        );
    }

    #[test]
    fn choice_set_trailing_at_eof_is_ok() {
        let src = "# S\n\nhi\n\n- [Go](#Go)\n";
        let (_y, blocks, errs) = walk_dialog(src);
        assert!(errs.is_empty(), "a choice set at end of input is trailing");
        assert!(blocks.iter().any(|b| b.kind == BlockKind::Choices));
    }

    #[test]
    fn two_choice_sets_in_one_section_is_non_trailing() {
        // A `-` list then a `*` list are two separate lists (CommonMark starts a
        // new list when the bullet marker changes), so two choice-set blocks land
        // in one section — the first is non-trailing.
        let src = "# S\n\n- [A](#A)\n\n* [B](#B)\n\n# A\n\nx\n\n# B\n\ny\n";
        let (_y, _b, errs) = walk_dialog(src);
        assert!(
            errs.iter()
                .any(|e| matches!(e, ChoiceSetError::NonTrailing { .. })),
            "a second list in the section makes the first non-trailing: {errs:?}"
        );
    }
}
