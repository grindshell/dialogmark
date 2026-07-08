//! Runtime stepper for gameplay.
//!
//! [`DialogWalker`] is constructed via [`Dialog::walk`](crate::Dialog::walk)
//! (fresh start) or [`Dialog::resume`](crate::Dialog::resume) (resumed from a
//! saved snapshot at a chosen heading). Two advance surfaces share one block
//! stepper:
//!
//! - [`DialogWalker::advance`] runs forward to the next **paragraph**, choice
//!   point, or termination — the streaming / visual-novel surface.
//! - [`DialogWalker::advance_segment`] runs forward to the next choice point or
//!   termination, **collecting** the narration walked along the way — the
//!   choice-per-turn surface a host's interaction session drives
//!   (CHOICES_AND_SEGMENTED_WALK.md §4).
//!
//! The walker never owns an `mlua::Lua` — it borrows one for the duration of
//! each call. Each walk runs its blocks in a **per-walk environment** chained to
//! the caller-supplied base env, so many walks share one VM without colliding
//! (§4.1). Game-specific Luau modules (`@grindshell/fs` etc.) are the caller's
//! responsibility, injected via that base env.

use mlua::{Lua, Table, Value};

use crate::blocks::{BlockKind, DialogBlock};
use crate::error::DialogError;
use crate::exec::{self, MAX_SIMULATION_STEPS, NextDirective};
use crate::state::{DialogState, PresentedOption};

/// The reserved option id of the synthetic "Continue" choice
/// [`DialogWalker::advance_page`] surfaces at a heading boundary (the paged-walk
/// page break). It is not an author-defined option — the caller resumes at the
/// option's `target` like any other — and cannot collide with an author option,
/// since a page break only fires where the section presented no options at all.
pub const PAGE_ADVANCE_ID: &str = "__page__";

/// The player-facing label of the synthetic page-advance option.
const PAGE_ADVANCE_LABEL: &str = "Continue";

/// One narration element collected while walking a segment
/// (CHOICES_AND_SEGMENTED_WALK.md §4.3). Borrows from the parsed
/// [`Dialog`](crate::Dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Narration<'a> {
    /// A paragraph block's text.
    Paragraph(&'a str),
    /// A heading block's text (for consumers that render section headers).
    Heading(&'a str),
}

/// One step of the per-paragraph walk: a paragraph to render, a choice point to
/// present, or the terminal reason the walk stopped. Once a walker yields
/// `Terminated`, all subsequent `advance` calls return the same payload.
#[derive(Debug, Clone, PartialEq)]
pub enum WalkStep<'a> {
    /// The next paragraph in the dialog, borrowed from the parsed
    /// [`Dialog`](crate::Dialog).
    Paragraph(&'a str),
    /// A choice point (a scripted `present` or a trailing choice set): the
    /// options to surface. The walk waits to be resumed
    /// ([`Dialog::resume`](crate::Dialog::resume)).
    Present(Vec<PresentedOption>),
    /// The walk has stopped. The payload says why.
    Terminated(TerminationReason),
}

/// Why a segment stopped (CHOICES_AND_SEGMENTED_WALK.md §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentStop {
    /// A choice point — options to show, then resume at the chosen target.
    Present(Vec<PresentedOption>),
    /// The walk ended (or faulted); the reason mirrors `advance`'s.
    Terminated(TerminationReason),
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
    /// either missing or doesn't match any heading, or a resume named a heading
    /// that doesn't exist. `name` is `None` if `t = "goto"` carried no `name`.
    GotoTargetMissing { name: Option<String> },
    /// Visited more than [`MAX_SIMULATION_STEPS`] blocks; almost certainly a
    /// goto loop.
    StepLimit,
    /// A code block raised a Luau syntax or runtime error, or `state.next` was
    /// shaped invalidly, or `state.next.t` was an unsupported value.
    ExecutionError { message: String },
    /// A paragraph or choice set appeared before the first heading in the
    /// source (only code blocks may form the prelude).
    PreludeInvalid { block_idx: usize, line: usize },
}

/// What processing one block produced — the shared unit both advance surfaces
/// drive their loop with.
enum Step<'a> {
    /// A paragraph or heading text to surface / collect.
    Narration(Narration<'a>),
    /// A choice point: pause with these options.
    Present(Vec<PresentedOption>),
    /// The cursor moved internally (a heading, a no-redirect code block, or a
    /// resolved goto); keep going.
    Advanced,
    /// The walk ended.
    Terminated(TerminationReason),
}

/// Stepper over a parsed [`Dialog`](crate::Dialog). See module docs.
pub struct DialogWalker<'a> {
    blocks: &'a [DialogBlock],
    state_t: Table,
    /// The per-walk environment blocks run in (carries `state`, chains to the
    /// caller's base env). Helper globals a block assigns live here.
    env: Table,
    cursor: usize,
    start_idx: usize,
    steps: usize,
    current_heading: Option<String>,
    previous_heading: Option<String>,
    prelude_ran: bool,
    /// True for a resumed walk — the prelude re-runs (helper setup) but its
    /// `state.next` and the cursor it would set are ignored (§4.2).
    resuming: bool,
    /// The chosen option's id for a resume, applied to `state.choice` after the
    /// prelude (so prelude code blocks don't consume it).
    pending_choice: Option<String>,
    /// True while a resume's `state.choice` is still live (set after the
    /// prelude, cleared by the first post-prelude code block that reads it).
    choice_live: bool,
    done: Option<TerminationReason>,
}

impl<'a> DialogWalker<'a> {
    /// Fresh-start walker. Allocates a new state table and per-walk env; the
    /// prelude runs on the first advance call.
    pub(crate) fn new(
        blocks: &'a [DialogBlock],
        lua: &Lua,
        base_env: Table,
        start_idx: usize,
    ) -> Result<Self, DialogError> {
        let state_t = exec::create_state_table(lua)?;
        let env = exec::create_walk_env(lua, &base_env, &state_t)?;
        Ok(Self {
            blocks,
            state_t,
            env,
            cursor: start_idx,
            start_idx,
            steps: 0,
            current_heading: None,
            previous_heading: None,
            prelude_ran: false,
            resuming: false,
            pending_choice: None,
            choice_live: false,
            done: None,
        })
    }

    /// Resume a segmented walk from a saved [`DialogState`] at the chosen
    /// option's target heading (CHOICES_AND_SEGMENTED_WALK.md §4.4). Re-runs the
    /// prelude into a fresh per-walk env, restores extras, sets
    /// `state.choice = choice_id`, and positions the cursor at `resume_heading`
    /// (a missing heading defers to a `GotoTargetMissing` on the first advance).
    pub(crate) fn resume(
        blocks: &'a [DialogBlock],
        lua: &Lua,
        base_env: Table,
        state: DialogState,
        resume_heading: &str,
        choice_id: Option<String>,
    ) -> Result<Self, DialogError> {
        let state_t = exec::create_state_table(lua)?;
        exec::seed_extras(lua, &state_t, &state.extras)?;
        let env = exec::create_walk_env(lua, &base_env, &state_t)?;

        let (cursor, done) = match exec::resolve_goto(blocks, resume_heading) {
            Some(target) => (target.idx, None),
            None => (
                blocks.len(),
                Some(TerminationReason::GotoTargetMissing {
                    name: Some(resume_heading.to_string()),
                }),
            ),
        };
        let (current_heading, previous_heading) = exec::prescan_headings(blocks, cursor);

        Ok(Self {
            blocks,
            state_t,
            env,
            cursor,
            start_idx: cursor,
            steps: 0,
            current_heading,
            previous_heading,
            prelude_ran: false,
            resuming: true,
            pending_choice: choice_id,
            choice_live: false,
            done,
        })
    }

    /// Advance the per-paragraph walk to the next paragraph, choice point, or
    /// termination (idempotent once terminated). `Err` is reserved for VM
    /// plumbing failures Dialogmark can't recover from.
    pub fn advance(&mut self, lua: &Lua) -> Result<WalkStep<'a>, DialogError> {
        if let Some(r) = &self.done {
            return Ok(WalkStep::Terminated(r.clone()));
        }
        self.ensure_started(lua);
        if let Some(r) = &self.done {
            return Ok(WalkStep::Terminated(r.clone()));
        }

        loop {
            if self.cursor >= self.blocks.len() {
                return Ok(self.terminate(TerminationReason::EndOfDialog));
            }
            if self.steps >= MAX_SIMULATION_STEPS {
                return Ok(self.terminate(TerminationReason::StepLimit));
            }
            self.steps += 1;

            match self.step_block(lua) {
                Step::Narration(Narration::Paragraph(p)) => return Ok(WalkStep::Paragraph(p)),
                // `advance` surfaces only paragraphs; heading texts are internal.
                Step::Narration(Narration::Heading(_)) => continue,
                Step::Present(opts) => return Ok(WalkStep::Present(opts)),
                Step::Advanced => continue,
                Step::Terminated(r) => return Ok(self.terminate(r)),
            }
        }
    }

    /// Advance one segment — to the next choice point or termination — returning
    /// the narration walked along the way plus why it stopped
    /// (CHOICES_AND_SEGMENTED_WALK.md §4.3). The choice-per-turn surface.
    pub fn advance_segment(
        &mut self,
        lua: &Lua,
    ) -> Result<(Vec<Narration<'a>>, SegmentStop), DialogError> {
        let mut narration: Vec<Narration<'a>> = Vec::new();
        if let Some(r) = &self.done {
            return Ok((narration, SegmentStop::Terminated(r.clone())));
        }
        self.ensure_started(lua);
        if let Some(r) = &self.done {
            return Ok((narration, SegmentStop::Terminated(r.clone())));
        }

        loop {
            if self.cursor >= self.blocks.len() {
                let r = self.finish(TerminationReason::EndOfDialog);
                return Ok((narration, SegmentStop::Terminated(r)));
            }
            if self.steps >= MAX_SIMULATION_STEPS {
                let r = self.finish(TerminationReason::StepLimit);
                return Ok((narration, SegmentStop::Terminated(r)));
            }
            self.steps += 1;

            match self.step_block(lua) {
                Step::Narration(n) => narration.push(n),
                Step::Present(opts) => return Ok((narration, SegmentStop::Present(opts))),
                Step::Advanced => continue,
                Step::Terminated(r) => {
                    let r = self.finish(r);
                    return Ok((narration, SegmentStop::Terminated(r)));
                }
            }
        }
    }

    /// Advance one **page** — like [`advance_segment`](Self::advance_segment),
    /// but also pausing at a heading boundary (CHOICES_AND_SEGMENTED_WALK.md §4.3
    /// "Paged walk"). When a section falls through to the next `#` heading with no
    /// choice point and no `state.next`, the walk stops and returns
    /// `SegmentStop::Present` with a single synthetic [`PAGE_ADVANCE_ID`]
    /// "Continue" option targeting that heading, so a linear dialog pages one
    /// section per call. A `goto` is page-transparent (its landing section opens
    /// as this page's own section); an explicit `present` / choice set / `exit` /
    /// end-of-dialog behaves exactly as in `advance_segment`.
    pub fn advance_page(
        &mut self,
        lua: &Lua,
    ) -> Result<(Vec<Narration<'a>>, SegmentStop), DialogError> {
        let mut narration: Vec<Narration<'a>> = Vec::new();
        if let Some(r) = &self.done {
            return Ok((narration, SegmentStop::Terminated(r.clone())));
        }
        self.ensure_started(lua);
        if let Some(r) = &self.done {
            return Ok((narration, SegmentStop::Terminated(r.clone())));
        }

        // True once this page's own opening heading has been consumed; the *next*
        // heading reached by fall-through is then a page boundary. A goto clears
        // it (the jumped-to heading opens a fresh section, no break), so an
        // explicit redirect stays page-transparent — only a natural fall-through
        // into the following section pages.
        let mut page_opened = false;
        loop {
            if self.cursor >= self.blocks.len() {
                let r = self.finish(TerminationReason::EndOfDialog);
                return Ok((narration, SegmentStop::Terminated(r)));
            }
            if self.steps >= MAX_SIMULATION_STEPS {
                let r = self.finish(TerminationReason::StepLimit);
                return Ok((narration, SegmentStop::Terminated(r)));
            }
            // A heading reached after this page's opening heading is the next
            // section: page here with a synthetic Continue, leaving the cursor on
            // the heading so the resume lands at its text (like the unadvanced
            // cursor at a `Choices` pause).
            if page_opened && self.blocks[self.cursor].kind == BlockKind::Heading {
                let opt = PresentedOption {
                    id: PAGE_ADVANCE_ID.to_string(),
                    label: PAGE_ADVANCE_LABEL.to_string(),
                    target: self.blocks[self.cursor].text.clone(),
                    note: None,
                    disabled: false,
                };
                return Ok((narration, SegmentStop::Present(vec![opt])));
            }

            let before = self.cursor;
            self.steps += 1;
            match self.step_block(lua) {
                Step::Narration(n @ Narration::Heading(_)) => {
                    page_opened = true;
                    narration.push(n);
                }
                Step::Narration(p) => narration.push(p),
                Step::Present(opts) => return Ok((narration, SegmentStop::Present(opts))),
                Step::Advanced => {
                    // A goto/jump (cursor moved by other than +1) opens a fresh
                    // page section: the heading it lands on is consumed, not paged.
                    if self.cursor != before + 1 {
                        page_opened = false;
                    }
                    continue;
                }
                Step::Terminated(r) => {
                    let r = self.finish(r);
                    return Ok((narration, SegmentStop::Terminated(r)));
                }
            }
        }
    }

    /// Snapshot the current state — suitable for save points. Includes idx,
    /// headings, and all user-defined extras. The managed `state.next` /
    /// `state.choice` are intentionally not surfaced.
    pub fn snapshot(&self) -> DialogState {
        exec::build_state_snapshot(
            &self.state_t,
            self.cursor,
            &self.current_heading,
            &self.previous_heading,
            None,
        )
    }

    /// Current cursor position (the block index the next step will visit).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Run the prelude (lazily, on first advance), then activate a resume's
    /// `state.choice` so the prelude's own code blocks don't consume it.
    fn ensure_started(&mut self, lua: &Lua) {
        if self.prelude_ran {
            return;
        }
        let resuming = self.resuming;
        self.run_prelude(lua, resuming);
        if self.done.is_some() {
            return;
        }
        match self.pending_choice.take() {
            Some(id) => {
                let _ = self.state_t.set("choice", id);
                self.choice_live = true;
            }
            None => {
                let _ = self.state_t.set("choice", Value::Nil);
            }
        }
    }

    /// Execute the prelude (code blocks before the first heading). On a fresh
    /// walk the last block's `state.next` can redirect / terminate and the
    /// cursor is positioned afterward; on a resume the prelude is helper setup
    /// only — its `state.next` and cursor are ignored (§4.2).
    fn run_prelude(&mut self, lua: &Lua, resuming: bool) {
        debug_assert!(!self.prelude_ran);
        self.prelude_ran = true;

        let first_heading = exec::first_heading_idx(self.blocks);

        // Only code blocks may precede the first heading.
        for b in &self.blocks[..first_heading] {
            if b.kind != BlockKind::Code {
                self.done = Some(TerminationReason::PreludeInvalid {
                    block_idx: b.idx,
                    line: b.start_line,
                });
                return;
            }
        }

        let mut last_next: Option<NextDirective> = None;
        for b in &self.blocks[..first_heading] {
            match exec::run_code_block(
                lua,
                &self.env,
                &self.state_t,
                &b.text,
                b.idx,
                b.idx,
                &None,
                &None,
            ) {
                Ok(next_opt) => last_next = next_opt,
                Err(msg) => {
                    self.done = Some(TerminationReason::ExecutionError { message: msg });
                    return;
                }
            }
        }

        if resuming {
            // The resume already positioned the cursor / headings at the chosen
            // heading; the prelude is pure helper setup here.
            return;
        }

        // Walk never starts before the first heading — prelude blocks have
        // already executed; visiting them again would double-fire.
        let walk_start_default = self.start_idx.max(first_heading);
        match last_next {
            None => self.cursor = walk_start_default,
            Some(next) => {
                if let Some(reason) = self.apply_directive_for_prelude(next) {
                    self.done = Some(reason);
                    return;
                }
            }
        }
        let (cur, prev) = exec::prescan_headings(self.blocks, self.cursor);
        self.current_heading = cur;
        self.previous_heading = prev;
    }

    /// Apply a prelude block's `state.next`. `present` is not a valid prelude
    /// directive (no narration context yet).
    fn apply_directive_for_prelude(&mut self, d: NextDirective) -> Option<TerminationReason> {
        match d {
            NextDirective::Exit => Some(TerminationReason::Exit),
            NextDirective::Goto { name } => {
                match name
                    .as_deref()
                    .and_then(|n| exec::resolve_goto(self.blocks, n))
                {
                    Some(t) => {
                        self.cursor = t.idx;
                        None
                    }
                    None => Some(TerminationReason::GotoTargetMissing { name }),
                }
            }
            NextDirective::Present(_) => Some(TerminationReason::ExecutionError {
                message: "state.next = { t = \"present\" } is not allowed in the prelude"
                    .to_string(),
            }),
        }
    }

    /// Process the block at the cursor, returning what it produced.
    fn step_block(&mut self, lua: &Lua) -> Step<'a> {
        let block: &'a DialogBlock = &self.blocks[self.cursor];
        match block.kind {
            BlockKind::Heading => {
                self.previous_heading = self.current_heading.take();
                self.current_heading = Some(block.text.clone());
                self.cursor += 1;
                Step::Narration(Narration::Heading(block.text.as_str()))
            }
            BlockKind::Paragraph => {
                self.cursor += 1;
                Step::Narration(Narration::Paragraph(block.text.as_str()))
            }
            BlockKind::Choices => {
                // A trailing choice set: present its options (id = target,
                // disabled = false, note = None — §4.3). The walk pauses here;
                // a resume jumps elsewhere, so the cursor is not advanced.
                let opts = block
                    .choices
                    .as_ref()
                    .map(|items| {
                        items
                            .iter()
                            .map(|c| PresentedOption {
                                id: c.target.clone(),
                                label: c.label.clone(),
                                target: c.target.clone(),
                                note: None,
                                disabled: false,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Step::Present(opts)
            }
            BlockKind::Code => {
                let result = exec::run_code_block(
                    lua,
                    &self.env,
                    &self.state_t,
                    &block.text,
                    block.idx,
                    self.cursor,
                    &self.current_heading,
                    &self.previous_heading,
                );
                // A live resume choice is consumed by the first post-prelude
                // code block, then cleared so it can't leak into a later block.
                if self.choice_live {
                    let _ = self.state_t.set("choice", Value::Nil);
                    self.choice_live = false;
                }
                match result {
                    Err(msg) => {
                        Step::Terminated(TerminationReason::ExecutionError { message: msg })
                    }
                    Ok(None) => {
                        self.cursor += 1;
                        Step::Advanced
                    }
                    Ok(Some(NextDirective::Exit)) => Step::Terminated(TerminationReason::Exit),
                    Ok(Some(NextDirective::Goto { name })) => {
                        match name
                            .as_deref()
                            .and_then(|n| exec::resolve_goto(self.blocks, n))
                        {
                            Some(target) => {
                                self.cursor = target.idx;
                                Step::Advanced
                            }
                            None => Step::Terminated(TerminationReason::GotoTargetMissing { name }),
                        }
                    }
                    Ok(Some(NextDirective::Present(opts))) => Step::Present(opts),
                }
            }
        }
    }

    /// Mark the walk terminated and return the `advance` payload.
    fn terminate(&mut self, reason: TerminationReason) -> WalkStep<'a> {
        self.done = Some(reason.clone());
        WalkStep::Terminated(reason)
    }

    /// Mark the walk terminated and return the reason (the `advance_segment`
    /// payload helper).
    fn finish(&mut self, reason: TerminationReason) -> TerminationReason {
        self.done = Some(reason.clone());
        reason
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

    /// A base env whose `__index` falls through to the VM globals — the minimal
    /// caller env (no host modules) the runtime/editor hand in.
    fn base_env(lua: &Lua) -> Table {
        let t = lua.create_table().unwrap();
        let mt = lua.create_table().unwrap();
        mt.set("__index", lua.globals()).unwrap();
        t.set_metatable(Some(mt)).unwrap();
        t
    }

    fn collect_until_terminated<'a>(
        walker: &mut DialogWalker<'a>,
        lua: &Lua,
    ) -> (Vec<String>, TerminationReason) {
        let mut paragraphs = Vec::new();
        loop {
            match walker.advance(lua).expect("advance should not Err") {
                WalkStep::Paragraph(p) => paragraphs.push(p.to_string()),
                WalkStep::Present(_) => panic!("unexpected choice point"),
                WalkStep::Terminated(r) => return (paragraphs, r),
            }
        }
    }

    #[test]
    fn walk_linear_yields_each_paragraph() {
        let d = parse("# Section\n\npara one\n\npara two\n\npara three\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert_eq!(ps, vec!["para one", "para two", "para three"]);
        assert_eq!(r, TerminationReason::EndOfDialog);
    }

    #[test]
    fn walk_pauses_only_on_paragraph() {
        let d = parse("# H\n\n```luau\n-- noop\n```\n\nbody\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "body"),
            other => panic!("unexpected: {other:?}"),
        }
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
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert_eq!(ps, vec!["farewell"]);
        assert_eq!(r, TerminationReason::EndOfDialog);
    }

    #[test]
    fn walk_exit_terminates() {
        let d =
            parse("# Start\n\n```luau\nstate.next = { t = \"exit\" }\n```\n\nshould not appear\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ps, r) = collect_until_terminated(&mut w, &lua);
        assert!(ps.is_empty(), "exit should terminate before paragraph");
        assert_eq!(r, TerminationReason::Exit);
    }

    #[test]
    fn walk_goto_missing_target() {
        let d = parse("# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Nope\" }\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
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
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::StepLimit) => {}
            other => panic!("expected StepLimit, got {other:?}"),
        }
    }

    #[test]
    fn walk_execution_error_lua_syntax() {
        let d = parse("# A\n\n```luau\nstate.x =\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
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
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
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
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
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
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "far body"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn walk_prelude_paragraph_invalid() {
        let d = parse("narration\n\n# Scene\n\nbody\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Terminated(TerminationReason::PreludeInvalid { block_idx, .. }) => {
                assert_eq!(block_idx, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn walk_managed_fields_clear_next_each_block() {
        let d = parse(
            "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"Target\" }\n```\n\n# Target\n\n```luau\nif state.next ~= nil then error(\"next leaked: \" .. tostring(state.next)) end\n```\n\nafter\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        match w.advance(&lua).unwrap() {
            WalkStep::Paragraph(p) => assert_eq!(p, "after"),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn walk_repeated_advance_after_termination_returns_same_terminator() {
        let d = parse("# A\n\n```luau\nstate.next = { t = \"exit\" }\n```\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        assert_eq!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::Exit)
        );
        assert_eq!(
            w.advance(&lua).unwrap(),
            WalkStep::Terminated(TerminationReason::Exit)
        );
    }

    // --- segmented walk + choice sets (CHOICES_AND_SEGMENTED_WALK.md §4) ------

    fn narration_text(ns: &[Narration<'_>]) -> Vec<String> {
        ns.iter()
            .map(|n| match n {
                Narration::Paragraph(s) | Narration::Heading(s) => s.to_string(),
            })
            .collect()
    }

    #[test]
    fn segment_collects_narration_then_presents_choice_set() {
        let d = parse(
            "# Greeting\n\nA drillmaster leans on a stave.\n\n- [Who are you?](#Who)\n- [Teach me.](#Learn)\n\n# Who\n\nI am the drillmaster.\n\n# Learn\n\nGood.\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ns, stop) = w.advance_segment(&lua).unwrap();
        assert_eq!(
            narration_text(&ns),
            vec!["Greeting", "A drillmaster leans on a stave."]
        );
        match stop {
            SegmentStop::Present(opts) => {
                let ids: Vec<&str> = opts.iter().map(|o| o.id.as_str()).collect();
                assert_eq!(ids, vec!["Who", "Learn"]);
                assert_eq!(opts[0].label, "Who are you?");
                assert_eq!(opts[0].target, "Who");
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn segment_resume_at_choice_target() {
        let d = parse(
            "# Greeting\n\nHello.\n\n- [Who are you?](#Who)\n- [Bye.](#Leave)\n\n# Who\n\nI am the drillmaster.\n\n```luau\nstate.next = { t = \"exit\" }\n```\n\n# Leave\n\nFarewell.\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (_ns, stop) = w.advance_segment(&lua).unwrap();
        let opts = match stop {
            SegmentStop::Present(o) => o,
            other => panic!("expected Present, got {other:?}"),
        };
        let snap = w.snapshot();
        let chosen = &opts[0]; // "Who"
        let mut w2 = d
            .resume(
                &lua,
                base_env(&lua),
                snap,
                &chosen.target,
                Some(chosen.id.clone()),
            )
            .unwrap();
        let (ns, stop) = w2.advance_segment(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["Who", "I am the drillmaster."]);
        assert_eq!(stop, SegmentStop::Terminated(TerminationReason::Exit));
    }

    #[test]
    fn segment_scripted_present_and_state_choice() {
        let d = parse(
            "# Menu\n\n```luau\nstate.next = { t = \"present\", options = {\n  { id = \"a\", label = \"Buy A\", target = \"Buy\" },\n  { id = \"b\", label = \"Buy B\", target = \"Buy\", note = \"pricey\", disabled = true },\n} }\n```\n\n# Buy\n\n```luau\nstate.bought = state.choice\n```\n\nDone.\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (_ns, stop) = w.advance_segment(&lua).unwrap();
        let opts = match stop {
            SegmentStop::Present(o) => o,
            other => panic!("expected Present, got {other:?}"),
        };
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[1].note.as_deref(), Some("pricey"));
        assert!(opts[1].disabled);

        let snap = w.snapshot();
        let mut w2 = d
            .resume(&lua, base_env(&lua), snap, "Buy", Some("a".to_string()))
            .unwrap();
        let (ns, stop) = w2.advance_segment(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["Buy", "Done."]);
        assert_eq!(
            stop,
            SegmentStop::Terminated(TerminationReason::EndOfDialog)
        );
        // The code under #Buy read state.choice ("a") into an extra.
        assert_eq!(
            w2.snapshot().extras.get("bought"),
            Some(&serde_json::json!("a"))
        );
    }

    #[test]
    fn segment_state_choice_cleared_after_first_code_block() {
        // #Buy's first code reads state.choice; a later code block must see nil.
        let d = parse(
            "# Menu\n\n```luau\nstate.next = { t = \"present\", options = { { id = \"x\", label = \"X\", target = \"Buy\" } } }\n```\n\n# Buy\n\n```luau\nstate.first = state.choice\n```\n\n```luau\nstate.second = state.choice\nif state.choice ~= nil then error(\"choice leaked\") end\n```\n\nok\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let _ = w.advance_segment(&lua).unwrap();
        let snap = w.snapshot();
        let mut w2 = d
            .resume(&lua, base_env(&lua), snap, "Buy", Some("x".to_string()))
            .unwrap();
        let (_ns, stop) = w2.advance_segment(&lua).unwrap();
        assert_eq!(
            stop,
            SegmentStop::Terminated(TerminationReason::EndOfDialog)
        );
        assert_eq!(
            w2.snapshot().extras.get("first"),
            Some(&serde_json::json!("x"))
        );
    }

    #[test]
    fn resume_prelude_reruns_and_is_idempotent() {
        // Replaces the old walk_with_state_skips_prelude: with per-walk envs the
        // prelude's helpers can't survive a resume, so it MUST re-run. A
        // side-effect-free prelude (helper defs only) is therefore idempotent —
        // a resumed segment can still call the helper.
        let d = parse(
            "```luau\nfunction greet() return \"hi\" end\n```\n\n# Menu\n\n```luau\nstate.next = { t = \"present\", options = { { id = \"g\", label = \"Go\", target = \"Use\" } } }\n```\n\n# Use\n\n```luau\nstate.said = greet()\n```\n\nok\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let _ = w.advance_segment(&lua).unwrap();
        let snap = w.snapshot();
        // The saved snapshot has NO live Luau (greet is gone); resume re-runs
        // the prelude to re-establish it, so #Use's greet() resolves.
        let mut w2 = d
            .resume(&lua, base_env(&lua), snap, "Use", Some("g".to_string()))
            .unwrap();
        let (_ns, stop) = w2.advance_segment(&lua).unwrap();
        assert_eq!(
            stop,
            SegmentStop::Terminated(TerminationReason::EndOfDialog)
        );
        assert_eq!(
            w2.snapshot().extras.get("said"),
            Some(&serde_json::json!("hi"))
        );
    }

    #[test]
    fn resume_missing_heading_is_goto_target_missing() {
        let d = parse("# A\n\nbody\n");
        let lua = Lua::new();
        let snap = DialogState::default();
        let mut w = d
            .resume(&lua, base_env(&lua), snap, "Nope", Some("x".to_string()))
            .unwrap();
        let (ns, stop) = w.advance_segment(&lua).unwrap();
        assert!(ns.is_empty());
        match stop {
            SegmentStop::Terminated(TerminationReason::GotoTargetMissing { name }) => {
                assert_eq!(name.as_deref(), Some("Nope"));
            }
            other => panic!("expected GotoTargetMissing, got {other:?}"),
        }
    }

    #[test]
    fn per_walk_envs_do_not_collide_on_one_vm() {
        // Two concurrent walks on the SAME vm: each defines a helper global; the
        // other must not see it (per-walk env isolation, §4.1).
        let d1 =
            parse("```luau\nMARK = \"one\"\n```\n\n# S\n\n```luau\nstate.seen = MARK\n```\n\nx\n");
        let d2 =
            parse("```luau\nMARK = \"two\"\n```\n\n# S\n\n```luau\nstate.seen = MARK\n```\n\nx\n");
        let lua = Lua::new();
        let mut w1 = d1.walk(&lua, base_env(&lua), 0).unwrap();
        let mut w2 = d2.walk(&lua, base_env(&lua), 0).unwrap();
        // Interleave: start w1, then w2, then finish w1.
        let _ = w1.advance(&lua).unwrap();
        let _ = w2.advance(&lua).unwrap();
        assert_eq!(
            w1.snapshot().extras.get("seen"),
            Some(&serde_json::json!("one"))
        );
        assert_eq!(
            w2.snapshot().extras.get("seen"),
            Some(&serde_json::json!("two"))
        );
        // Nothing leaked into the shared VM globals.
        assert!(lua.globals().get::<Value>("MARK").unwrap().is_nil());
    }

    #[test]
    fn segment_surfaces_show_heading_override() {
        // A code block's `state.show_heading` lands on the snapshot as a managed
        // field (a per-segment heading-visibility override), not as an extra.
        let d = parse("# S\n\n```luau\nstate.show_heading = true\n```\n\nbody.\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let _ = w.advance_segment(&lua).unwrap();
        let snap = w.snapshot();
        assert_eq!(snap.show_heading, Some(true));
        assert!(
            !snap.extras.contains_key("show_heading"),
            "show_heading is managed, not an author extra"
        );
    }

    #[test]
    fn show_heading_override_resets_each_segment() {
        // Section A opts its heading in; section B does not. Resuming into B must
        // see `None` — the override never rides `extras`, so it can't leak forward.
        let d = parse(
            "# A\n\n```luau\nstate.show_heading = true\nstate.next = { t = \"present\", options = { { id = \"go\", label = \"Go\", target = \"B\" } } }\n```\n\n# B\n\nbody.\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let _ = w.advance_segment(&lua).unwrap();
        let snap = w.snapshot();
        assert_eq!(snap.show_heading, Some(true));

        let mut w2 = d
            .resume(&lua, base_env(&lua), snap, "B", Some("go".to_string()))
            .unwrap();
        let _ = w2.advance_segment(&lua).unwrap();
        assert_eq!(
            w2.snapshot().show_heading,
            None,
            "the per-segment override does not carry into the next segment"
        );
    }

    // --- paged walk (advance_page, CHOICES_AND_SEGMENTED_WALK.md §4.3) ---------

    #[test]
    fn page_walk_breaks_at_each_heading() {
        let d =
            parse("# One\n\nfirst page.\n\n# Two\n\nsecond page.\n\n# Three\n\nthird page.\n");
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();

        // Page 1: the opening section, paused with a synthetic Continue to #Two.
        let (ns, stop) = w.advance_page(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["One", "first page."]);
        let opts = match stop {
            SegmentStop::Present(o) => o,
            other => panic!("expected a page break, got {other:?}"),
        };
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].id, PAGE_ADVANCE_ID);
        assert_eq!(opts[0].label, "Continue");
        assert_eq!(opts[0].target, "Two");

        // Page 2: resume at #Two, paused with a Continue to #Three.
        let snap = w.snapshot();
        let mut w2 = d
            .resume(
                &lua,
                base_env(&lua),
                snap,
                "Two",
                Some(PAGE_ADVANCE_ID.to_string()),
            )
            .unwrap();
        let (ns, stop) = w2.advance_page(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["Two", "second page."]);
        let opts = match stop {
            SegmentStop::Present(o) => o,
            other => panic!("expected a page break, got {other:?}"),
        };
        assert_eq!(opts[0].target, "Three");

        // Page 3: the final section walks off the end and closes.
        let snap = w2.snapshot();
        let mut w3 = d
            .resume(
                &lua,
                base_env(&lua),
                snap,
                "Three",
                Some(PAGE_ADVANCE_ID.to_string()),
            )
            .unwrap();
        let (ns, stop) = w3.advance_page(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["Three", "third page."]);
        assert_eq!(
            stop,
            SegmentStop::Terminated(TerminationReason::EndOfDialog)
        );
    }

    #[test]
    fn page_walk_honors_explicit_choice_set() {
        // A paged section that ends in a trailing link-list presents that list,
        // not a synthetic Continue — paging only fills a fall-through gap.
        let d = parse(
            "# Start\n\npick one.\n\n- [Left](#Left)\n- [Right](#Right)\n\n# Left\n\nl\n\n# Right\n\nr\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ns, stop) = w.advance_page(&lua).unwrap();
        assert_eq!(narration_text(&ns), vec!["Start", "pick one."]);
        match stop {
            SegmentStop::Present(opts) => {
                let ids: Vec<&str> = opts.iter().map(|o| o.id.as_str()).collect();
                assert_eq!(ids, vec!["Left", "Right"]);
            }
            other => panic!("expected the authored choice set, got {other:?}"),
        }
    }

    #[test]
    fn page_walk_goto_is_transparent() {
        // A `goto` jumps without a page break: #Start's code sends the walk to
        // #End, whose narration joins the same page, which then runs off the end.
        let d = parse(
            "# Start\n\n```luau\nstate.next = { t = \"goto\", name = \"End\" }\n```\n\n# Skipped\n\nnope\n\n# End\n\ndone.\n",
        );
        let lua = Lua::new();
        let mut w = d.walk(&lua, base_env(&lua), 0).unwrap();
        let (ns, stop) = w.advance_page(&lua).unwrap();
        // "Start" heading, then (goto, no break) the "End" heading + its body.
        assert_eq!(narration_text(&ns), vec!["Start", "End", "done."]);
        assert_eq!(
            stop,
            SegmentStop::Terminated(TerminationReason::EndOfDialog)
        );
    }
}
