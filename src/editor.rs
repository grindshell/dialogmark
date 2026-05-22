//! Editor-side surface. Compiled only with the `editor` Cargo feature.
//!
//! Aggregates *all* frontmatter errors and renders the markdown body to HTML,
//! so the editor's Validate button can show every problem at once.

use pulldown_cmark::{Event, MetadataBlockKind, Options, Parser, Tag, TagEnd, html};
use serde::{Deserialize, Serialize};

use crate::error::FrontmatterError;
use crate::frontmatter::{DialogFrontmatter, parse_frontmatter_lenient};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDialog {
    pub frontmatter: DialogFrontmatter,
    pub html: String,
    /// Non-fatal diagnostics (unknown keys, missing/invalid name, list-shaped
    /// author, quoted values, etc). The dialog is still returned with whatever
    /// was parsed so the UI can show the file alongside the warnings.
    pub parse_errors: Vec<FrontmatterError>,
}

/// Parse a dialog markdown file. Lifts the YAML frontmatter out, parses it,
/// and renders the remaining body to HTML.
pub fn parse_dialog(content: &str) -> ParsedDialog {
    let parser = Parser::new_ext(content, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);

    let mut yaml = String::new();
    let mut in_metadata = false;
    let mut body_events: Vec<Event> = Vec::new();

    for event in parser {
        match &event {
            Event::Start(Tag::MetadataBlock(MetadataBlockKind::YamlStyle)) => {
                in_metadata = true;
            }
            Event::End(TagEnd::MetadataBlock(MetadataBlockKind::YamlStyle)) => {
                in_metadata = false;
            }
            _ if in_metadata => {
                if let Event::Text(t) = &event {
                    yaml.push_str(t);
                }
            }
            _ => body_events.push(event),
        }
    }

    let (frontmatter, errors) = parse_frontmatter_lenient(&yaml);

    let mut html_out = String::new();
    html::push_html(&mut html_out, body_events.into_iter());

    ParsedDialog {
        frontmatter,
        html: html_out,
        parse_errors: errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file() {
        let p = parse_dialog("");
        assert!(p.html.is_empty());
        assert!(p.frontmatter.name.is_empty());
        assert_eq!(p.parse_errors.len(), 1);
        assert_eq!(p.parse_errors[0], FrontmatterError::MissingName);
    }

    #[test]
    fn body_only_no_frontmatter() {
        let p = parse_dialog("# Hello\n\nworld\n");
        assert!(p.html.contains("<h1>Hello</h1>"));
        assert!(p.html.contains("<p>world</p>"));
        assert_eq!(p.frontmatter.name, "");
        assert!(
            p.parse_errors
                .iter()
                .any(|e| matches!(e, FrontmatterError::MissingName))
        );
    }

    #[test]
    fn frontmatter_only() {
        let p = parse_dialog("---\nname: Greet\ndescription: hi\n---\n");
        assert_eq!(p.frontmatter.name, "Greet");
        assert_eq!(p.frontmatter.description.as_deref(), Some("hi"));
        assert!(p.parse_errors.is_empty());
    }

    #[test]
    fn frontmatter_and_body() {
        let src = "---\nname: Greet\n---\n\n# Hi\n";
        let p = parse_dialog(src);
        assert_eq!(p.frontmatter.name, "Greet");
        assert!(p.html.contains("<h1>Hi</h1>"));
        assert!(!p.html.contains("name:"));
        assert!(!p.html.contains("---"));
    }

    #[test]
    fn collects_multiple_frontmatter_errors() {
        let src = "---\nname: A\nfoo: bar\ndescription: \"hi\"\n---\n";
        let p = parse_dialog(src);
        assert_eq!(p.frontmatter.name, "A");
        assert_eq!(p.parse_errors.len(), 2);
        assert!(
            p.parse_errors
                .iter()
                .any(|e| matches!(e, FrontmatterError::UnknownKey { key } if key == "foo"))
        );
        assert!(
            p.parse_errors.iter().any(
                |e| matches!(e, FrontmatterError::QuotedValue { key } if key == "description")
            )
        );
    }

    #[test]
    fn luau_fenced_block_round_trips() {
        let src = "---\nname: A\n---\n\n```luau\nprint(\"hi\")\n```\n";
        let p = parse_dialog(src);
        assert!(p.html.contains("<code class=\"language-luau\">"));
        assert!(p.html.contains("print(\"hi\")"));
    }
}
