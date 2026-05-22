//! Editor-side simulator. Compiled only with the `editor` Cargo feature.
//!
//! [`simulate_dialog`] is a one-shot walk of a dialog file. Unlike the runtime
//! stepper ([`crate::DialogWalker`]) it:
//!
//! - parses the markdown body itself via [`crate::extract_blocks`] — no
//!   frontmatter check, so authors can validate broken files in the editor;
//! - records a [`DialogTraceEntry`] for every block visited (including prelude
//!   blocks), with a `source_preview` and a `state_after` snapshot;
//! - never returns an `Err` — VM failures, syntax errors, missing goto
//!   targets, runaway loops, and prelude validation failures all materialize
//!   as a populated `terminated_reason` / `terminated_message`.
//!
//! Termination reason strings ("end_of_dialog", "exit", "goto_target_missing",
//! "step_limit", "execution_error", "prelude_invalid") match what the editor
//! frontend already consumes.

use mlua::Lua;
use serde::Serialize;

use crate::blocks::{BlockKind, DialogBlock, extract_blocks};
use crate::exec::{
    self, MAX_SIMULATION_STEPS, build_state_snapshot, preview_source, refresh_managed_fields,
};
use crate::state::{DialogNext, DialogState};

#[derive(Debug, Clone, Serialize)]
pub struct DialogTraceEntry {
    pub block_idx: usize,
    /// `"heading"`, `"paragraph"`, or `"code"`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_text: Option<String>,
    pub source_preview: String,
    /// State snapshot after this block was processed.
    pub state_after: DialogState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_error: Option<String>,
    /// True when the entry came from prelude auto-execution rather than the
    /// main walk. Prelude entries always appear before walk entries.
    pub is_prelude: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DialogSimulationResult {
    pub blocks_total: usize,
    pub trace: Vec<DialogTraceEntry>,
    pub final_state: DialogState,
    pub terminated_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminated_message: Option<String>,
}

/// Walk `content` from `start_idx`, executing every fenced code block as
/// `function(state) … end` against the caller-supplied `lua` VM. Returns the
/// per-block trace and the final state. See the module docs for prelude /
/// termination semantics.
pub fn simulate_dialog(content: &str, lua: &Lua, start_idx: usize) -> DialogSimulationResult {
    let blocks = extract_blocks(content);
    let blocks_total = blocks.len();
    let first_heading_idx = exec::first_heading_idx(&blocks);

    // Reject prelude paragraphs before any Luau executes.
    for b in &blocks[..first_heading_idx] {
        if b.kind == BlockKind::Paragraph {
            return DialogSimulationResult {
                blocks_total,
                trace: vec![],
                final_state: DialogState {
                    idx: b.idx,
                    ..Default::default()
                },
                terminated_reason: "prelude_invalid".to_string(),
                terminated_message: Some(format!(
                    "paragraph at block #{} (line {}) is not allowed before the first heading; \
                     prelude must contain only code blocks",
                    b.idx, b.start_line
                )),
            };
        }
    }

    let state_t = match exec::create_state_table(lua) {
        Ok(t) => t,
        Err(e) => {
            return DialogSimulationResult {
                blocks_total,
                trace: vec![],
                final_state: DialogState::default(),
                terminated_reason: "execution_error".to_string(),
                terminated_message: Some(format!("failed to create state table: {e}")),
            };
        }
    };

    let mut trace: Vec<DialogTraceEntry> = Vec::new();
    let mut last_prelude_next: Option<DialogNext> = None;

    // Prelude: run every code block in order. Per Dialogmark divergence, the
    // last block's state.next can redirect or terminate the walk (the editor
    // reference only records it).
    for b in &blocks[..first_heading_idx] {
        debug_assert_eq!(b.kind, BlockKind::Code);
        let mut entry = DialogTraceEntry {
            block_idx: b.idx,
            kind: "code".to_string(),
            heading_text: None,
            source_preview: preview_source(&b.text),
            state_after: DialogState {
                idx: b.idx,
                ..Default::default()
            },
            execution_error: None,
            is_prelude: true,
        };

        match exec::run_code_block(lua, &state_t, &b.text, b.idx, b.idx, &None, &None) {
            Ok(next_opt) => {
                entry.state_after =
                    build_state_snapshot(&state_t, b.idx, &None, &None, next_opt.clone());
                last_prelude_next = next_opt;
            }
            Err(msg) => {
                entry.execution_error = Some(msg.clone());
                entry.state_after = build_state_snapshot(&state_t, b.idx, &None, &None, None);
                trace.push(entry);
                let final_state = build_state_snapshot(&state_t, b.idx, &None, &None, None);
                return DialogSimulationResult {
                    blocks_total,
                    trace,
                    final_state,
                    terminated_reason: "execution_error".to_string(),
                    terminated_message: Some(msg),
                };
            }
        }
        trace.push(entry);
    }

    // Determine where the walk starts. Default: max(start_idx, first_heading_idx).
    // Prelude's last state.next can override.
    let walk_start_default = start_idx.max(first_heading_idx);
    let walk_start_result = resolve_after_prelude(last_prelude_next, &blocks, walk_start_default);

    let walk_start = match walk_start_result {
        Ok(c) => c,
        Err((reason, message)) => {
            let final_state =
                build_state_snapshot(&state_t, walk_start_default, &None, &None, None);
            return DialogSimulationResult {
                blocks_total,
                trace,
                final_state,
                terminated_reason: reason,
                terminated_message: message,
            };
        }
    };

    // Refresh managed fields so post-prelude snapshots aren't polluted by
    // stale `next` values from the last prelude block.
    let _ = refresh_managed_fields(&state_t, walk_start, &None, &None);

    if walk_start >= blocks_total {
        let (current, previous) = exec::prescan_headings(&blocks, walk_start);
        let final_state = build_state_snapshot(&state_t, walk_start, &current, &previous, None);
        return DialogSimulationResult {
            blocks_total,
            trace,
            final_state,
            terminated_reason: "end_of_dialog".to_string(),
            terminated_message: None,
        };
    }

    let (mut current_heading, mut previous_heading) = exec::prescan_headings(&blocks, walk_start);

    let mut cursor = walk_start;
    let mut steps: usize = 0;
    let mut terminated_reason = "end_of_dialog".to_string();
    let mut terminated_message: Option<String> = None;
    let mut last_next: Option<DialogNext> = None;

    loop {
        if cursor >= blocks_total {
            terminated_reason = "end_of_dialog".to_string();
            break;
        }
        if steps >= MAX_SIMULATION_STEPS {
            terminated_reason = "step_limit".to_string();
            terminated_message = Some(format!(
                "exceeded {MAX_SIMULATION_STEPS} visited blocks; likely an infinite goto loop"
            ));
            break;
        }
        steps += 1;

        let block = &blocks[cursor];
        let mut entry = DialogTraceEntry {
            block_idx: cursor,
            kind: match block.kind {
                BlockKind::Heading => "heading",
                BlockKind::Paragraph => "paragraph",
                BlockKind::Code => "code",
            }
            .to_string(),
            heading_text: if block.kind == BlockKind::Heading {
                Some(block.text.clone())
            } else {
                None
            },
            source_preview: preview_source(&block.text),
            state_after: DialogState::default(),
            execution_error: None,
            is_prelude: false,
        };

        let mut block_next: Option<DialogNext> = None;
        let mut hit_error = false;

        match block.kind {
            BlockKind::Heading => {
                previous_heading = current_heading.take();
                current_heading = Some(block.text.clone());
            }
            BlockKind::Paragraph => {}
            BlockKind::Code => {
                match exec::run_code_block(
                    lua,
                    &state_t,
                    &block.text,
                    block.idx,
                    cursor,
                    &current_heading,
                    &previous_heading,
                ) {
                    Ok(next_opt) => {
                        block_next = next_opt;
                    }
                    Err(msg) => {
                        entry.execution_error = Some(msg.clone());
                        terminated_reason = "execution_error".to_string();
                        terminated_message = Some(msg);
                        hit_error = true;
                    }
                }
            }
        }

        entry.state_after = build_state_snapshot(
            &state_t,
            cursor,
            &current_heading,
            &previous_heading,
            block_next.clone(),
        );
        last_next = block_next.clone();
        trace.push(entry);

        if hit_error {
            break;
        }

        if let Some(n) = &block_next {
            match n.t.as_str() {
                "exit" => {
                    terminated_reason = "exit".to_string();
                    break;
                }
                "goto" => {
                    let Some(target_name) = &n.name else {
                        terminated_reason = "goto_target_missing".to_string();
                        terminated_message = Some(
                            "state.next.t == 'goto' but state.next.name is missing".to_string(),
                        );
                        break;
                    };
                    match exec::resolve_goto(&blocks, target_name) {
                        Some(target) => {
                            cursor = target.idx;
                        }
                        None => {
                            terminated_reason = "goto_target_missing".to_string();
                            terminated_message = Some(format!("no heading named {target_name:?}"));
                            break;
                        }
                    }
                }
                other => {
                    terminated_reason = "execution_error".to_string();
                    terminated_message = Some(format!("unsupported state.next.t {other:?}"));
                    break;
                }
            }
        } else {
            cursor += 1;
            if cursor >= blocks_total {
                terminated_reason = "end_of_dialog".to_string();
                break;
            }
        }
    }

    let final_state = build_state_snapshot(
        &state_t,
        cursor,
        &current_heading,
        &previous_heading,
        last_next,
    );

    DialogSimulationResult {
        blocks_total,
        trace,
        final_state,
        terminated_reason,
        terminated_message,
    }
}

/// Decide where the walk should start after the prelude has run. Returns the
/// new cursor on success, or `(terminated_reason, terminated_message)` to
/// surface as the simulation result.
fn resolve_after_prelude(
    last_next: Option<DialogNext>,
    blocks: &[DialogBlock],
    walk_start_default: usize,
) -> Result<usize, (String, Option<String>)> {
    match last_next {
        None => Ok(walk_start_default),
        Some(n) => match n.t.as_str() {
            "exit" => Err(("exit".to_string(), None)),
            "goto" => {
                let Some(name) = n.name else {
                    return Err((
                        "goto_target_missing".to_string(),
                        Some("state.next.t == 'goto' but state.next.name is missing".to_string()),
                    ));
                };
                match exec::resolve_goto(blocks, &name) {
                    Some(target) => Ok(target.idx),
                    None => Err((
                        "goto_target_missing".to_string(),
                        Some(format!("no heading named {name:?}")),
                    )),
                }
            }
            other => Err((
                "execution_error".to_string(),
                Some(format!("unsupported state.next.t {other:?}")),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(src: &str, start_idx: usize) -> DialogSimulationResult {
        let lua = Lua::new();
        simulate_dialog(src, &lua, start_idx)
    }

    #[test]
    fn simulate_linear_advances_through_blocks() {
        let src = "# Section\n\npara one\n\npara two\n\npara three\n";
        let r = sim(src, 0);
        assert_eq!(r.blocks_total, 4);
        assert_eq!(r.trace.len(), 4);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        let visited: Vec<usize> = r.trace.iter().map(|t| t.block_idx).collect();
        assert_eq!(visited, vec![0, 1, 2, 3]);
        assert!(r.trace.iter().all(|t| !t.is_prelude));
    }

    #[test]
    fn simulate_goto_jumps_to_named_heading() {
        let src = "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Bye\" }\n```\n\n# Skipped\n\nignored paragraph\n\n# Bye\n\nfarewell\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        let visited: Vec<usize> = r.trace.iter().map(|t| t.block_idx).collect();
        assert_eq!(visited, vec![0, 1, 4, 5]);
        assert_eq!(r.final_state.current_heading.as_deref(), Some("Bye"));
        assert_eq!(r.final_state.previous_heading.as_deref(), Some("Start"));
    }

    #[test]
    fn simulate_goto_missing_target_terminates_with_error() {
        let src =
            "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"DoesNotExist\" }\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "goto_target_missing");
        assert!(
            r.terminated_message
                .as_deref()
                .unwrap_or("")
                .contains("DoesNotExist")
        );
        assert_eq!(r.trace.len(), 2);
        assert_eq!(r.trace[1].block_idx, 1);
    }

    #[test]
    fn simulate_starts_at_user_idx_with_prescanned_headings() {
        let src = "# First\n\nPara1\n\n# Second\n\nPara2\n\nPara3\n";
        let r = sim(src, 3);
        assert_eq!(r.trace.len(), 2);
        let first = &r.trace[0];
        assert_eq!(first.block_idx, 3);
        assert_eq!(first.state_after.current_heading.as_deref(), Some("Second"));
        assert_eq!(first.state_after.previous_heading.as_deref(), Some("First"));
        assert_eq!(r.terminated_reason, "end_of_dialog");
    }

    #[test]
    fn simulate_code_with_syntax_error_terminates() {
        let src = "# Start\n\npara before\n\n```luau\nstate.x = \n```\n\npara after\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "execution_error");
        assert_eq!(r.trace.len(), 3);
        let code_entry = &r.trace[2];
        assert_eq!(code_entry.block_idx, 2);
        assert!(code_entry.execution_error.is_some());
        assert!(!code_entry.is_prelude);
    }

    #[test]
    fn simulate_step_limit_terminates() {
        let src = "# A\n\n```luau\nstate.next = { t = \"goto\", name = \"B\" }\n```\n\n# B\n\n```luau\nstate.next = { t = \"goto\", name = \"A\" }\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "step_limit");
        assert!(r.trace.len() >= MAX_SIMULATION_STEPS);
        assert!(
            r.terminated_message
                .as_deref()
                .unwrap_or("")
                .contains("1000")
        );
    }

    #[test]
    fn simulate_shared_lua_state_across_blocks() {
        let src = "```luau\nshared_value = 7\n```\n\n# Section\n\n```luau\nif shared_value ~= 7 then error(\"shared_value missing\") end\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert_eq!(r.trace.len(), 3);
        assert!(r.trace[0].is_prelude);
        assert!(!r.trace[1].is_prelude);
        assert!(!r.trace[2].is_prelude);
        assert!(r.trace.iter().all(|t| t.execution_error.is_none()));
    }

    #[test]
    fn simulate_starts_past_end_is_immediate_eof() {
        let r = sim("# Title\n\nbody\n", 99);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert!(r.trace.is_empty());
        assert_eq!(r.blocks_total, 2);
    }

    #[test]
    fn simulate_resets_next_between_blocks() {
        let src = "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Target\" }\n```\n\n# Target\n\n```luau\n-- no-op; previous goto should NOT carry over\n```\n\nafter\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        let visited: Vec<usize> = r.trace.iter().map(|t| t.block_idx).collect();
        assert_eq!(visited, vec![0, 1, 2, 3, 4]);
        assert!(r.trace[3].state_after.next.is_none());
    }

    // --- prelude tests ----------------------------------------------------

    #[test]
    fn simulate_prelude_paragraph_is_invalid() {
        let src = "narration about the scene\n\n# Scene\n\nbody\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "prelude_invalid");
        assert!(r.trace.is_empty());
        let msg = r.terminated_message.as_deref().unwrap_or("");
        assert!(msg.contains("paragraph") && msg.contains("first heading"));
    }

    #[test]
    fn simulate_only_prelude_no_headings() {
        let src = "```luau\nx = 1\n```\n\n```luau\ny = 2\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert_eq!(r.trace.len(), 2);
        assert!(r.trace.iter().all(|t| t.is_prelude));
        assert!(r.final_state.current_heading.is_none());
    }

    #[test]
    fn simulate_prelude_syntax_error_aborts_before_walk() {
        let src = "```luau\nbroken =\n```\n\n# Scene\n\nbody\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "execution_error");
        assert_eq!(r.trace.len(), 1);
        assert!(r.trace[0].is_prelude);
        assert!(r.trace[0].execution_error.is_some());
    }

    #[test]
    fn simulate_user_idx_inside_prelude_snaps_to_first_heading() {
        let src = "```luau\nseen = (seen or 0) + 1\n```\n\n# Scene\n\nbody\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert_eq!(r.trace.len(), 3);
        assert!(r.trace[0].is_prelude);
        assert_eq!(r.trace[0].block_idx, 0);
        assert_eq!(r.trace[1].block_idx, 1);
        assert!(!r.trace[1].is_prelude);
        assert_eq!(r.trace[2].block_idx, 2);
    }

    // --- extras ----------------------------------------------------------

    #[test]
    fn simulate_extras_persist_across_blocks_and_appear_in_final_state() {
        let src = "# Start\n\n```luau\nstate.player = \"Bob\"\nstate.flags = { greeted = true, age = 30 }\n```\n\n# Next\n\n```luau\nif state.player ~= \"Bob\" then error(\"player lost\") end\nif not state.flags.greeted then error(\"flag lost\") end\nstate.counter = (state.counter or 0) + 1\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        let extras = &r.final_state.extras;
        assert_eq!(extras.get("player"), Some(&serde_json::json!("Bob")));
        assert_eq!(extras.get("counter"), Some(&serde_json::json!(1)));
        let flags = extras.get("flags").expect("flags");
        assert_eq!(flags["greeted"], serde_json::json!(true));
        assert_eq!(flags["age"], serde_json::json!(30));
    }

    #[test]
    fn simulate_prelude_extras_carry_into_walk() {
        let src = "```luau\nstate.player_name = \"Hero\"\n```\n\n# Section\n\n```luau\nif state.player_name ~= \"Hero\" then error(\"name lost\") end\nstate.greeted = true\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert_eq!(
            r.final_state.extras.get("player_name"),
            Some(&serde_json::json!("Hero"))
        );
        assert_eq!(
            r.final_state.extras.get("greeted"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn simulate_managed_field_mutations_do_not_leak_across_blocks() {
        let src = "# A\n\n```luau\nstate.idx = 999\nstate.current_heading = \"hijacked\"\nstate.tag = \"present\"\n```\n\n# B\n\n```luau\nif state.idx ~= 3 then error(\"idx leaked: \" .. tostring(state.idx)) end\nif state.current_heading ~= \"B\" then error(\"heading leaked: \" .. tostring(state.current_heading)) end\nif state.tag ~= \"present\" then error(\"custom field lost\") end\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        assert_eq!(
            r.final_state.extras.get("tag"),
            Some(&serde_json::json!("present"))
        );
        assert!(!r.final_state.extras.contains_key("idx"));
        assert!(!r.final_state.extras.contains_key("current_heading"));
        assert_eq!(r.final_state.current_heading.as_deref(), Some("B"));
    }

    #[test]
    fn simulate_extras_array_form_serializes_as_json_array() {
        let src = "# A\n\n```luau\nstate.items = { \"sword\", \"shield\", \"potion\" }\n```\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        let items = r.final_state.extras.get("items").expect("items");
        assert_eq!(items, &serde_json::json!(["sword", "shield", "potion"]));
    }

    // --- Dialogmark-specific divergences ----------------------------------

    #[test]
    fn simulate_exit_redirect_in_walk_terminates() {
        let src = "# A\n\n```luau\nstate.next = { t = \"exit\" }\n```\n\nshould not appear\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "exit");
        // Only heading + code blocks were visited; the paragraph after was skipped.
        let visited: Vec<usize> = r.trace.iter().map(|t| t.block_idx).collect();
        assert_eq!(visited, vec![0, 1]);
    }

    #[test]
    fn simulate_prelude_exit_terminates() {
        let src = "```luau\nstate.next = { t = \"exit\" }\n```\n\n# Scene\n\nbody\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "exit");
        // Only the prelude block is in the trace; the walk never started.
        assert_eq!(r.trace.len(), 1);
        assert!(r.trace[0].is_prelude);
    }

    #[test]
    fn simulate_prelude_goto_redirects() {
        let src = "```luau\nstate.next = { t = \"goto\", name = \"Far\" }\n```\n\n# Near\n\nnear body\n\n# Far\n\nfar body\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "end_of_dialog");
        // Skipped Near heading + paragraph; landed at Far heading + paragraph.
        // Trace: prelude (0), Far heading (3), far body (4).
        let visited: Vec<usize> = r.trace.iter().map(|t| t.block_idx).collect();
        assert_eq!(visited, vec![0, 3, 4]);
    }

    #[test]
    fn simulate_prelude_goto_missing_target() {
        let src =
            "```luau\nstate.next = { t = \"goto\", name = \"Nope\" }\n```\n\n# Scene\n\nbody\n";
        let r = sim(src, 0);
        assert_eq!(r.terminated_reason, "goto_target_missing");
        assert!(
            r.terminated_message
                .as_deref()
                .unwrap_or("")
                .contains("Nope")
        );
    }
}
