//! YAML-subset frontmatter parser for dialog files.
//!
//! Grammar (whitespace-significant):
//! - Blank lines and `# comment` lines are ignored.
//! - `key: value` — value is the rest of the line, trimmed. Plain string.
//! - `key: |` then indented continuation lines — newlines preserved.
//! - `key: >` then indented continuation lines — folded to spaces.
//!
//! Anything else — flow lists, block lists, nested mappings, quoted scalars,
//! or unknown keys — is a `FrontmatterError`. Two entry points share one
//! walker:
//!
//! - [`parse_frontmatter_failfast`] returns on the first error. Used by the
//!   gameplay runtime.
//! - [`parse_frontmatter_lenient`] (editor feature) collects every error and
//!   returns them with whatever was parsed, so the editor can surface all
//!   problems at once.

use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::error::FrontmatterError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogFrontmatter {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The frame **title** shown when this dialog drives a zone interaction
    /// (mud.md "Dialogue-driven interactions"). Authored here so the editor's
    /// generated `zone_interactions.luau` can lift it into each
    /// `reg:register_interaction` def, instead of the title being hand-written
    /// at registration time. Free text; optional (an interaction `title` is
    /// itself optional server-side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether each section's `#` heading renders **in the frame body** when this
    /// dialog drives an interaction (mud.md "Dialogue-driven interactions"). Off
    /// by default: the frame's panel title already carries the scene label, so the
    /// section heading is redundant clutter in the body. Set `show_headings: true`
    /// to surface every section's heading as an in-body header (e.g. a paged,
    /// tour-style dialog whose pages each want a visible title). The value is a
    /// boolean flag: exactly `true` enables it; anything else (or absence) leaves
    /// it off.
    #[serde(default)]
    pub show_headings: bool,
    /// Collapsed `author` OR `authors` field. The design constraint forbids
    /// YAML lists here — both keys are always a single string or a multiline
    /// string scalar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// Parse the frontmatter YAML, stopping at the first error.
pub fn parse_frontmatter_failfast(yaml: &str) -> Result<DialogFrontmatter, FrontmatterError> {
    let mut first_err: Option<FrontmatterError> = None;
    let fm = parse_frontmatter_walker(yaml, &mut |err| {
        first_err = Some(err);
        ControlFlow::Break(())
    });
    match first_err {
        Some(err) => Err(err),
        None => Ok(fm),
    }
}

/// Parse the frontmatter YAML, collecting every error encountered. Returns the
/// best-effort parsed frontmatter alongside the diagnostics.
#[cfg(feature = "editor")]
pub fn parse_frontmatter_lenient(yaml: &str) -> (DialogFrontmatter, Vec<FrontmatterError>) {
    let mut errors: Vec<FrontmatterError> = Vec::new();
    let fm = parse_frontmatter_walker(yaml, &mut |err| {
        errors.push(err);
        ControlFlow::Continue(())
    });
    (fm, errors)
}

/// Non-empty + all ASCII alphanumeric or underscore. Matches the `system_name`
/// convention used elsewhere in Grindshell (`^[A-Za-z0-9_]+$`).
fn is_valid_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Shared YAML-subset walker. `on_error` decides whether to keep going
/// (editor) or bail immediately (runtime). The walker honors `Break` by
/// returning whatever it has parsed without running further validation.
fn parse_frontmatter_walker(
    yaml: &str,
    on_error: &mut dyn FnMut(FrontmatterError) -> ControlFlow<()>,
) -> DialogFrontmatter {
    let mut fm = DialogFrontmatter::default();
    let mut saw_name = false;
    let mut saw_show_headings = false;
    let mut saw_author = false;
    let mut saw_authors = false;
    let mut bailed = false;

    // Helper: emit an error and return `true` if the caller asked to stop.
    // Used inside the loop (sets `bailed`) and post-loop (does not).
    let mut emit_brk = |err: FrontmatterError| -> bool { on_error(err).is_break() };
    macro_rules! emit {
        ($err:expr) => {{
            if emit_brk($err) {
                bailed = true;
                true
            } else {
                false
            }
        }};
    }

    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;
    'outer: while i < lines.len() {
        let raw = lines[i];
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        // Any indented non-blank line at the outer level is a fatal parse
        // error. Block-scalar continuation lines (`|` / `>`) are consumed by
        // the inner loop below, so a hit here means a list, nested mapping,
        // or other YAML shape we don't accept.
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if emit!(FrontmatterError::UnparseableLine {
                line: raw.to_string(),
            }) {
                break 'outer;
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("- ") || trimmed == "-" {
            if emit!(FrontmatterError::ListValue {
                key: "<top-level>".to_string(),
            }) {
                break 'outer;
            }
            i += 1;
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            if emit!(FrontmatterError::UnparseableLine {
                line: trimmed.to_string(),
            }) {
                break 'outer;
            }
            i += 1;
            continue;
        };
        let key = trimmed[..colon].trim().to_string();
        let after = trimmed[colon + 1..].trim();

        if after.starts_with('[') {
            if emit!(FrontmatterError::ListValue { key: key.clone() }) {
                break 'outer;
            }
            i += 1;
            continue;
        }

        let value: String = if after == "|" || after == ">" {
            let join_with_newlines = after == "|";
            let mut collected: Vec<String> = Vec::new();
            i += 1;
            while i < lines.len() {
                let cont = lines[i];
                let cont_trim = cont.trim_end();
                if cont_trim.is_empty() {
                    collected.push(String::new());
                    i += 1;
                    continue;
                }
                // Continuation lines must be indented. The first
                // non-indented line ends the block scalar.
                if !cont.starts_with(' ') && !cont.starts_with('\t') {
                    break;
                }
                let stripped = cont_trim.trim_start();
                if (stripped.starts_with("- ") || stripped == "-")
                    && emit!(FrontmatterError::ListValue { key: key.clone() })
                {
                    break 'outer;
                }
                collected.push(stripped.to_string());
                i += 1;
            }
            while collected.last().map(|s| s.is_empty()).unwrap_or(false) {
                collected.pop();
            }
            if join_with_newlines {
                collected.join("\n")
            } else {
                collected.join(" ")
            }
        } else {
            i += 1;
            let s = after;
            let is_quoted = (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
                || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2);
            if is_quoted && emit!(FrontmatterError::QuotedValue { key: key.clone() }) {
                break 'outer;
            }
            // Preserve raw value (including quotes) so subsequent passes /
            // previews still have something useful to show.
            s.to_string()
        };

        match key.as_str() {
            "name" => {
                if saw_name
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "name".to_string(),
                    })
                {
                    break 'outer;
                }
                saw_name = true;
                fm.name = value;
            }
            "description" => {
                if fm.description.is_some()
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "description".to_string(),
                    })
                {
                    break 'outer;
                }
                fm.description = Some(value);
            }
            "title" => {
                if fm.title.is_some()
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "title".to_string(),
                    })
                {
                    break 'outer;
                }
                fm.title = Some(value);
            }
            "show_headings" => {
                if saw_show_headings
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "show_headings".to_string(),
                    })
                {
                    break 'outer;
                }
                saw_show_headings = true;
                // Frontmatter values are strings; treat this as a boolean flag —
                // only an exact `true` (case-insensitive) enables it.
                fm.show_headings = value.trim().eq_ignore_ascii_case("true");
            }
            "author" => {
                if saw_author
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "author".to_string(),
                    })
                {
                    break 'outer;
                }
                saw_author = true;
                fm.author = Some(value);
            }
            "authors" => {
                if saw_authors
                    && emit!(FrontmatterError::DuplicateKey {
                        key: "authors".to_string(),
                    })
                {
                    break 'outer;
                }
                saw_authors = true;
                // `author` wins if both are present.
                if !saw_author {
                    fm.author = Some(value);
                }
            }
            other => {
                if emit!(FrontmatterError::UnknownKey {
                    key: other.to_string(),
                }) {
                    break 'outer;
                }
            }
        }
    }

    if bailed {
        return fm;
    }

    if saw_author && saw_authors && emit_brk(FrontmatterError::AuthorAliasConflict) {
        return fm;
    }
    if fm.name.is_empty() {
        let _ = emit_brk(FrontmatterError::MissingName);
    } else if !is_valid_name(&fm.name) {
        let _ = emit_brk(FrontmatterError::InvalidName {
            name: fm.name.clone(),
        });
    }

    fm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failfast(yaml: &str) -> Result<DialogFrontmatter, FrontmatterError> {
        parse_frontmatter_failfast(yaml)
    }

    #[test]
    fn empty_yaml_misses_name() {
        let err = failfast("").unwrap_err();
        assert_eq!(err, FrontmatterError::MissingName);
    }

    #[test]
    fn name_only_parses() {
        let fm = failfast("name: Greet\n").unwrap();
        assert_eq!(fm.name, "Greet");
        assert_eq!(fm.description, None);
        assert_eq!(fm.author, None);
    }

    #[test]
    fn description_parses() {
        let fm = failfast("name: A\ndescription: hi\n").unwrap();
        assert_eq!(fm.description.as_deref(), Some("hi"));
    }

    #[test]
    fn title_parses() {
        let fm = failfast("name: A\ntitle: Guild Outfitter\n").unwrap();
        assert_eq!(fm.title.as_deref(), Some("Guild Outfitter"));
    }

    #[test]
    fn duplicate_title_is_rejected() {
        let err = failfast("name: A\ntitle: One\ntitle: Two\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::DuplicateKey {
                key: "title".to_string()
            }
        );
    }

    #[test]
    fn show_headings_defaults_off() {
        let fm = failfast("name: A\n").unwrap();
        assert!(!fm.show_headings);
    }

    #[test]
    fn show_headings_true_enables() {
        let fm = failfast("name: A\nshow_headings: true\n").unwrap();
        assert!(fm.show_headings);
    }

    #[test]
    fn show_headings_non_true_stays_off() {
        let fm = failfast("name: A\nshow_headings: false\n").unwrap();
        assert!(!fm.show_headings);
    }

    #[test]
    fn duplicate_show_headings_is_rejected() {
        let err = failfast("name: A\nshow_headings: true\nshow_headings: true\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::DuplicateKey {
                key: "show_headings".to_string()
            }
        );
    }

    #[test]
    fn invalid_name_alphanumeric_only() {
        let err = failfast("name: has space\n").unwrap_err();
        assert!(matches!(err, FrontmatterError::InvalidName { .. }));
    }

    #[test]
    fn underscore_name_parses() {
        let fm = failfast("name: wire_tender\n").unwrap();
        assert_eq!(fm.name, "wire_tender");
    }

    #[test]
    fn rejects_inline_list_author() {
        let err = failfast("name: A\nauthor: [a, b]\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::ListValue {
                key: "author".to_string()
            }
        );
    }

    #[test]
    fn rejects_block_list_author() {
        let err = failfast("name: A\nauthor: |\n  - one\n  - two\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::ListValue {
                key: "author".to_string()
            }
        );
    }

    #[test]
    fn rejects_top_level_block_list() {
        let err = failfast("- one\nname: A\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::ListValue {
                key: "<top-level>".to_string()
            }
        );
    }

    #[test]
    fn multiline_pipe_preserves_newlines() {
        let fm = failfast("name: A\nauthor: |\n  Line one\n  Line two\n").unwrap();
        assert_eq!(fm.author.as_deref(), Some("Line one\nLine two"));
    }

    #[test]
    fn multiline_fold_joins_with_spaces() {
        let fm = failfast("name: A\nauthor: >\n  Line one\n  Line two\n").unwrap();
        assert_eq!(fm.author.as_deref(), Some("Line one Line two"));
    }

    #[test]
    fn unknown_key_rejected() {
        let err = failfast("name: A\nfoo: bar\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::UnknownKey {
                key: "foo".to_string()
            }
        );
    }

    #[test]
    fn authors_alias_is_accepted() {
        let fm = failfast("name: A\nauthors: Team\n").unwrap();
        assert_eq!(fm.author.as_deref(), Some("Team"));
    }

    #[test]
    fn frontmatter_unquoted_number_is_string() {
        let fm = failfast("name: 42\n").unwrap();
        assert_eq!(fm.name, "42");
    }

    // ---------- Dialogmark-specific divergences ----------

    #[test]
    fn quoted_value_is_rejected() {
        let err = failfast("name: A\ndescription: \"hi\"\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::QuotedValue {
                key: "description".to_string()
            }
        );
    }

    #[test]
    fn single_quoted_value_is_rejected() {
        let err = failfast("name: A\ndescription: 'hi'\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::QuotedValue {
                key: "description".to_string()
            }
        );
    }

    #[test]
    fn duplicate_author_is_rejected() {
        let err = failfast("name: A\nauthor: Alice\nauthor: Bob\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::DuplicateKey {
                key: "author".to_string()
            }
        );
    }

    #[test]
    fn author_and_authors_conflict_is_rejected() {
        let err = failfast("name: A\nauthor: Alice\nauthors: Bob\n").unwrap_err();
        assert_eq!(err, FrontmatterError::AuthorAliasConflict);
    }

    #[test]
    fn unparseable_line_is_rejected() {
        let err = failfast("name: A\nbogus line\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::UnparseableLine {
                line: "bogus line".to_string()
            }
        );
    }

    #[test]
    fn nested_mapping_is_rejected_as_unparseable() {
        let err = failfast("name: A\nauthor:\n  realname: Bob\n  role: writer\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::UnparseableLine {
                line: "  realname: Bob".to_string()
            }
        );
    }

    #[test]
    fn indented_line_with_tab_is_rejected() {
        let err = failfast("name: A\n\tindented\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::UnparseableLine {
                line: "\tindented".to_string()
            }
        );
    }

    // ---------- Lenient parser collects multiple ----------

    #[cfg(feature = "editor")]
    #[test]
    fn lenient_collects_multiple_errors() {
        let yaml = "name: A\nfoo: bar\ndescription: \"hi\"\n";
        let (fm, errs) = parse_frontmatter_lenient(yaml);
        assert_eq!(fm.name, "A");
        assert_eq!(errs.len(), 2);
        assert!(
            errs.iter()
                .any(|e| matches!(e, FrontmatterError::UnknownKey { key } if key == "foo"))
        );
        assert!(
            errs.iter().any(
                |e| matches!(e, FrontmatterError::QuotedValue { key } if key == "description")
            )
        );
    }

    #[cfg(feature = "editor")]
    #[test]
    fn lenient_missing_name_is_reported_once() {
        let (fm, errs) = parse_frontmatter_lenient("");
        assert_eq!(fm.name, "");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0], FrontmatterError::MissingName);
    }
}
