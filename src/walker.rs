//! Runtime stepper for gameplay.
//!
//! [`DialogWalker`] is constructed via [`Dialog::walk`](crate::Dialog::walk)
//! (fresh start) or [`Dialog::walk_with_state`](crate::Dialog::walk_with_state)
//! (resumed from a saved snapshot). Each call to [`DialogWalker::advance`]
//! runs the walk forward until the next paragraph is reached or the dialog
//! terminates — headings and code blocks are processed internally.
//!
//! The walker never owns an `mlua::Lua` — it borrows one for the duration of
//! each `advance` call. Game-specific Luau modules (`@grindshell/fs` etc.) are
//! the caller's responsibility.

use mlua::{Lua, Table};

use crate::blocks::{BlockKind, DialogBlock};
use crate::error::DialogError;
use crate::exec::{self, MAX_SIMULATION_STEPS};
use crate::state::{DialogNext, DialogState};

/// One step of the walk: either a paragraph for the game to render, or the
/// terminal reason the walk stopped. Once a walker yields `Terminated`, all
/// subsequent `advance` calls return the same `Terminated` payload.
#[derive(Debug, Clone)]
pub enum WalkStep<'a> {
    /// The next paragraph in the dialog, borrowed from the parsed
    /// [`Dialog`](crate::Dialog).
    Paragraph(&'a str),
    /// The walk has stopped. The payload says why.
    Terminated(TerminationReason),
}

impl PartialEq for WalkStep<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (WalkStep::Paragraph(a), WalkStep::Paragraph(b)) => a == b,
            (WalkStep::Terminated(a), WalkStep::Terminated(b)) => a == b,
            _ => false,
        }
    }
}

/// Why a walk stopped. Distinguishes successful completion (`EndOfDialog`,
/// `Exit`) from author / script faults (`GotoTargetMissing`, `ExecutionError`,
/// `PreludeInvalid`) and runaway loops (`StepLimit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationReason {
    /// Walked off the end of the block list.
    EndOfDialog,
    /// A code block set `state.next = { t = "exit" }`.
    Exit,
    /// A code block set `state.next = { t = "goto", name = X }` where `X` is
    /// either missing or doesn't match any heading. `name` is `None` if the
    /// script set `t = "goto"` without a `name` field.
    GotoTargetMissing { name: Option<String> },
    /// Visited more than [`MAX_SIMULATION_STEPS`] blocks; almost certainly a
    /// goto loop.
    StepLimit,
    /// A code block raised a Luau syntax or runtime error, or `state.next` was
    /// shaped invalidly, or `state.next.t` was an unsupported value.
    ExecutionError { message: String },
    /// A paragraph appeared before the first heading in the source.
    PreludeInvalid { block_idx: usize, line: usize },
}

/// Stepper over a parsed [`Dialog`](crate::Dialog). See module docs.
pub struct DialogWalker<'a> {
    blocks: &'a [DialogBlock],
    state_t: Table,
    cursor: usize,
    start_idx: usize,
    steps: usize,
    current_heading: Option<String>,
    previous_heading: Option<String>,
    prelude_ran: bool,
    done: Option<TerminationReason>,
}

impl<'a> DialogWalker<'a> {
    /// Fresh-start walker. Allocates a new state table; the prelude runs on
    /// the first call to [`DialogWalker::advance`].
    pub(crate) fn new(
        blocks: &'a [DialogBlock],
        lua: &Lua,
        start_idx: usize,
    ) -> Result<Self, DialogError> {
        let state_t = exec::create_state_table(lua)?;
        Ok(Self {
            blocks,
            state_t,
            cursor: start_idx,
            start_idx,
            steps: 0,
            current_heading: None,
            previous_heading: None,
            prelude_ran: false,
            done: None,
        })
    }

    /// Resume walker from a saved [`DialogState`]. The prelude is skipped (it
    /// already ran in the session that produced the snapshot) and the saved
    /// extras are pushed onto the fresh state table. The saved `state.next` is
    /// ignored — the cursor already reflects any post-goto position.
    pub(crate) fn from_state(
        blocks: &'a [DialogBlock],
        lua: &Lua,
        state: DialogState,
    ) -> Result<Self, DialogError> {
        let state_t = exec::create_state_table(lua)?;
        exec::seed_extras(lua, &state_t, &state.extras)?;
        Ok(Self {
            blocks,
            state_t,
            cursor: state.idx,
            start_idx: state.idx,
            steps: 0,
            current_heading: state.current_heading,
            previous_heading: state.previous_heading,
            prelude_ran: true,
            done: None,
        })
    }

    /// Advance the walk until the next paragraph or termination. Returns
    /// `Ok(WalkStep::Paragraph)` for paragraphs to render and
    /// `Ok(WalkStep::Terminated)` once the walk is over (idempotent once
    /// terminated). `Err` is reserved for VM plumbing failures Dialogmark
    /// can't recover from.
    pub fn advance(&mut self, lua: &Lua) -> Result<WalkStep<'a>, DialogError> {
        if let Some(ref r) = self.done {
            return Ok(WalkStep::Terminated(r.clone()));
        }

        if !self.prelude_ran {
            self.run_prelude(lua);
            if let Some(ref r) = self.done {
                return Ok(WalkStep::Terminated(r.clone()));
            }
        }

        loop {
            if self.cursor >= self.blocks.len() {
                return Ok(self.terminate(TerminationReason::EndOfDialog));
            }
            if self.steps >= MAX_SIMULATION_STEPS {
                return Ok(self.terminate(TerminationReason::StepLimit));
            }
            self.steps += 1;

            let block: &'a DialogBlock = &self.blocks[self.cursor];
            match block.kind {
                BlockKind::Heading => {
                    self.previous_heading = self.current_heading.take();
                    self.current_heading = Some(block.text.clone());
                    self.cursor += 1;
                }
                BlockKind::Paragraph => {
                    let p: &'a str = block.text.as_str();
                    self.cursor += 1;
                    return Ok(WalkStep::Paragraph(p));
                }
                BlockKind::Code => {
                    let result = exec::run_code_block(
                        lua,
                        &self.state_t,
                        &block.text,
                        block.idx,
                        self.cursor,
                        &self.current_heading,
                        &self.previous_heading,
                    );
                    match result {
                        Err(msg) => {
                            return Ok(
                                self.terminate(TerminationReason::ExecutionError { message: msg })
                            );
                        }
                        Ok(None) => self.cursor += 1,
                        Ok(Some(next)) => {
                            if let Some(reason) = self.apply_next(next) {
                                return Ok(self.terminate(reason));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Snapshot the current state — suitable for save points. Includes idx,
    /// headings, and all user-defined extras. `state.next` is intentionally
    /// not surfaced; the saved cursor already reflects any redirect.
    pub fn snapshot(&self) -> DialogState {
        exec::build_state_snapshot(
            &self.state_t,
            self.cursor,
            &self.current_heading,
            &self.previous_heading,
            None,
        )
    }

    /// Current cursor position (the block index `advance` will visit next).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn run_prelude(&mut self, lua: &Lua) {
        debug_assert!(!self.prelude_ran);
        self.prelude_ran = true;

        let first_heading = exec::first_heading_idx(self.blocks);

        for b in &self.blocks[..first_heading] {
            if b.kind == BlockKind::Paragraph {
                self.done = Some(TerminationReason::PreludeInvalid {
                    block_idx: b.idx,
                    line: b.start_line,
                });
                return;
            }
        }

        let mut last_next: Option<DialogNext> = None;
        for b in &self.blocks[..first_heading] {
            debug_assert_eq!(b.kind, BlockKind::Code);
            match exec::run_code_block(lua, &self.state_t, &b.text, b.idx, b.idx, &None, &None) {
                Ok(next_opt) => last_next = next_opt,
                Err(msg) => {
                    self.done = Some(TerminationReason::ExecutionError { message: msg });
                    return;
                }
            }
        }

        // Walk never starts before the first heading — prelude blocks have
        // already executed; visiting them again would double-fire.
        let walk_start_default = self.start_idx.max(first_heading);

        match last_next {
            None => {
                self.cursor = walk_start_default;
                let (cur, prev) = exec::prescan_headings(self.blocks, self.cursor);
                self.current_heading = cur;
                self.previous_heading = prev;
            }
            Some(next) => {
                if let Some(reason) = self.apply_next(next) {
                    self.done = Some(reason);
                    return;
                }
                let (cur, prev) = exec::prescan_headings(self.blocks, self.cursor);
                self.current_heading = cur;
                self.previous_heading = prev;
            }
        }
    }

    fn apply_next(&mut self, next: DialogNext) -> Option<TerminationReason> {
        match next.t.as_str() {
            "exit" => Some(TerminationReason::Exit),
            "goto" => match next
                .name
                .as_deref()
                .and_then(|n| exec::resolve_goto(self.blocks, n))
            {
                Some(target) => {
                    self.cursor = target.idx;
                    None
                }
                None => Some(TerminationReason::GotoTargetMissing { name: next.name }),
            },
            other => Some(TerminationReason::ExecutionError {
                message: format!("unsupported state.next.t {other:?}"),
            }),
        }
    }

    fn terminate(&mut self, reason: TerminationReason) -> WalkStep<'a> {
        self.done = Some(reason.clone());
        WalkStep::Terminated(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dialog;

    fn parse(src: &str) -> Dialog {
        let with_fm = format!("---\nname: T\n---\n\n{src}");
        Dialog::parse(&with_fm).expect("test source should parse")
    }

    fn collect_until_terminated<'a>(
        walker: &mut DialogWalker<'a>,
        lua: &Lua,
    ) -> (Vec<String>, TerminationReason) {
        let mut paragraphs = Vec::new();
        loop {
            match walker.advance(lua).expect("advance should not Err") {
                WalkStep::Paragraph(p) => paragraphs.push(p.to_string()),
                WalkStep::Terminated(r) => return (paragraphs, r),
            }
        }
    }

    #[test]
    fn walk_linear_yields_each_paragraph() {
        let d = parse("# Section\n\npara one\n\npara two\n\npara three\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert_eq!(ps, vec!["para one", "para two", "para three"]);
        assert_eq!(r, TerminationReason::EndOfDialog);
    }

    #[test]
    fn walk_pauses_only_on_paragraph() {
        // heading + code (no-op) + paragraph: advance() returns paragraph
        // directly; the heading and code are consumed internally.
        let d = parse("# H\n\n```luau\n-- noop\n```\n\nbody\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "body"),
            WalkStep::Terminated(r) => panic!("unexpected terminated: {r:?}"),
        }
        // After the paragraph, end of dialog.
        assert!(matches!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::EndOfDialog)
        ));
    }

    #[test]
    fn walk_goto_jumps_to_named_heading() {
        let d = parse(
            "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Bye\" }\n```\n\n# Skipped\n\nignored\n\n# Bye\n\nfarewell\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert_eq!(ps, vec!["farewell"]);
        assert_eq!(r, TerminationReason::EndOfDialog);
    }

    #[test]
    fn walk_exit_terminates() {
        let d =
            parse("# Start\n\n```luau\nstate.next = { t = \"exit\" }\n```\n\nshould not appear\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert!(ps.is_empty(), "exit should terminate before paragraph");
        assert_eq!(r, TerminationReason::Exit);
    }

    #[test]
    fn walk_goto_missing_target() {
        let d = parse("# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Nope\" }\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::GotoTargetMissing { name }) => {
                assert_eq!(name.as_deref(), Some("Nope"));
            }
            other => panic!("expected GotoTargetMissing, got {other:?}"),
        }
    }

    #[test]
    fn walk_step_limit_caught() {
        let d = parse(
            "# A\n\n```luau\nstate.next = { t = \"goto\", name = \"B\" }\n```\n\n# B\n\n```luau\nstate.next = { t = \"goto\", name = \"A\" }\n```\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::StepLimit) => {}
            other => panic!("expected StepLimit, got {other:?}"),
        }
    }

    #[test]
    fn walk_execution_error_lua_syntax() {
        let d = parse("# A\n\n```luau\nstate.x =\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::ExecutionError { message }) => {
                assert!(!message.is_empty(), "error message should be non-empty");
            }
            other => panic!("expected ExecutionError, got {other:?}"),
        }
    }

    #[test]
    fn walk_prelude_runs_before_first_paragraph() {
        let d = parse(
            "```luau\nstate.greeting = \"hello\"\n```\n\n# Scene\n\n```luau\nif state.greeting ~= \"hello\" then error(\"prelude lost\") end\n```\n\nbody\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "body"),
            other => panic!("unexpected: {other:?}"),
        }
        let snap = w.snapshot();
        assert_eq!(
            snap.extras.get("greeting"),
            Some(&serde_json::json!("hello"))
        );
    }

    #[test]
    fn walk_prelude_exit_terminates_immediately() {
        let d =
            parse("```luau\nstate.next = { t = \"exit\" }\n```\n\n# Scene\n\nshould not appear\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        assert_eq!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::Exit)
        );
    }

    #[test]
    fn walk_prelude_goto_overrides_start_idx() {
        let d = parse(
            "```luau\nstate.next = { t = \"goto\", name = \"Far\" }\n```\n\n# Near\n\nnear body\n\n# Far\n\nfar body\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "far body"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn walk_prelude_goto_missing_target() {
        let d = parse("```luau\nstate.next = { t = \"goto\", name = \"Nope\" }\n```\n\n# Scene\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::GotoTargetMissing { name }) => {
                assert_eq!(name.as_deref(), Some("Nope"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn walk_prelude_paragraph_invalid() {
        let d = parse("narration\n\n# Scene\n\nbody\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::PreludeInvalid { block_idx, .. }) => {
                assert_eq!(block_idx, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn walk_with_state_roundtrip_extras() {
        let d = parse(
            "# A\n\n```luau\nstate.counter = (state.counter or 0) + 1\n```\n\nfirst\n\nsecond\n",
        );
        let lua1 = Lua::new();
        let mut w = d.walk(&lua1, 0).unwrap();
        // First advance executes the code block, then yields "first".
        assert!(matches!(w.advance(&lua1).unwrap(), WalkStep::Paragraph(p) if p == "first"));
        let snap = w.snapshot();
        assert_eq!(snap.extras.get("counter"), Some(&serde_json::json!(1)));
        drop(w);

        // Resume in a fresh VM.
        let lua2 = Lua::new();
        let mut w2 = d.walk_with_state(&lua2, snap).unwrap();
        // Resumed walker should pick up at the next block ("second").
        match w2.advance(&lua2).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "second"),
            other => panic!("unexpected: {other:?}"),
        }
        // And the counter should still be readable.
        let snap2 = w2.snapshot();
        assert_eq!(snap2.extras.get("counter"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn walk_with_state_skips_prelude() {
        // Prelude bumps a counter. If walk_with_state re-ran it, the counter
        // would end at 2 instead of 1.
        let d = parse("```luau\nstate.tick = (state.tick or 0) + 1\n```\n\n# A\n\nfirst\n");
        let lua1 = Lua::new();
        let mut w = d.walk(&lua1, 0).unwrap();
        assert!(matches!(w.advance(&lua1).unwrap(), WalkStep::Paragraph(_)));
        let snap = w.snapshot();
        assert_eq!(snap.extras.get("tick"), Some(&serde_json::json!(1)));

        let lua2 = Lua::new();
        let w2 = d.walk_with_state(&lua2, snap).unwrap();
        let snap2 = w2.snapshot();
        // Prelude did not re-run, so tick is still 1.
        assert_eq!(snap2.extras.get("tick"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn walk_repeated_advance_after_termination_returns_same_terminator() {
        let d = parse("# A\n\n```luau\nstate.next = { t = \"exit\" }\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        assert_eq!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::Exit)
        );
        // Idempotent.
        assert_eq!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::Exit)
        );
    }

    #[test]
    fn walk_managed_fields_clear_next_each_block() {
        // First code block sets state.next to a goto. After the goto resolves
        // and we land at the target heading, the next code block (no-op) should
        // see state.next == nil (i.e. refresh_managed_fields cleared it).
        let d = parse(
            "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Target\" }\n```\n\n# Target\n\n```luau\nif state.next ~= nil then error(\"next leaked: \" .. tostring(state.next)) end\n```\n\nafter\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "after"),
            WalkStep::Terminated(r) => panic!("expected paragraph, got {r:?}"),
        }
    }
}
