# Choice sets and the segmented walk

Status: **implemented** (runtime + parser, 2026-06-27) — except the editor
interactive-preview parity in §5, which stays deferred (the linear
`simulate_dialog` stops at a choice point with reason `"present"`). This document
specifies the format and runtime additions that let a dialog drive a host's
*choice-per-turn* interaction session (a recruiter menu, a vendor, a
quest-giver) rather than only a linear paragraph stream.

Dialogmark stays **game-neutral**: nothing here names Grindshell, frames, NPCs,
or host capabilities. The motivating integration (how Grindshell's interaction
engine consumes this) lives in the Grindshell knowledge base
(`knowledge-base/design/plugins/dialogmark.md` and `mud.md`) and is intentionally
not duplicated here. What this crate owns is the **format** (how an author writes
a branch the player picks) and the **runtime contract** (how a caller advances
the walk one segment at a time on a shared VM).

Grounded against the current code: `src/blocks.rs` (parse), `src/walker.rs`
(runtime stepper), `src/exec.rs` (Luau execution), `src/state.rs` (persisted
state), `src/sim.rs` (editor simulator).

## 1. The gap this closes

Today the runtime walks a dialog as a paragraph stream:
[`DialogWalker::advance`](src/walker.rs) runs forward, processing headings and
code blocks internally, and pauses to yield each **paragraph**
(`WalkStep::Paragraph`). Branching is script-only: a code block sets
`state.next = { t = "goto", name = … }` or `{ t = "exit" }`.

A choice-per-turn consumer needs three things the format and runtime don't have:

1. A way for the dialog to **present labelled choices** and stop until the player
   picks one — both as plain authored Markdown (the simple case) and as
   script-computed options (the gated/dynamic case).
2. A way to **resume** at the chosen branch, carrying the dialog's scratch but
   **no live Luau state** — because a host that serves many players holds only
   serialized state between a player's turns, not a live VM per session.
3. A way to run many such walks on **one shared VM** without their globals
   colliding.

The three sections below add exactly these: **choice sets** (§2), the **`present`
redirect** and **`state.choice`** field (§3), and the **per-walk environment +
segmented stepper + resume** (§4).

## 2. Choice sets (format)

A **choice set** is a trailing Markdown link-list under a heading. It declares the
player's options as ordinary Markdown, with no script:

```markdown
# Greeting

A scarred drillmaster leans on a notched stave. "You want to learn something
that keeps you alive?"

- [Who are you?](#Who)
- [Teach me an art.](#Learn)
- [Not today.](#Leave)
```

Reaching this section presents three options; picking one resumes the walk at the
linked heading (`#Who`, `#Learn`, `#Leave`).

### Grammar

- A choice set is a **top-level unordered list** (not nested inside a blockquote,
  table, or another list) whose **every item is a single Markdown link** of the
  form `[label](#Target)`.
  - `label` is the option's player-facing text (the link text, trimmed).
  - `Target` is the **destination fragment**: a leading `#` followed by the
    **exact text** of a heading elsewhere in the dialog. Matching is the same
    exact-text rule [`exec::resolve_goto`](src/exec.rs) already uses for `goto`
    (no slug/anchor normalization, spaces preserved): `[Learn](#Learn an art)`
    targets the heading `# Learn an art`.
- A list whose items are **not all** single links is **not** a choice set. It is a
  malformed document, not ordinary prose: the parser raises (see §6), rather than
  silently skipping it as today's walker does for arbitrary lists.

### The trailing rule

A choice set must be **the last block of its heading's section** — nothing may
follow it within the section before the next heading (or end of input). A list
followed, in the same section, by a paragraph, a code block, or another list is
**non-trailing** and is a parse error.

Rationale: a choice set *stops* the walk. Any block after it in the same section
is unreachable, so authoring one is always a mistake; rejecting it at parse time
surfaces the mistake to the author instead of silently dropping content. (This is
the one place the parser gets *stricter* than "skip what you don't model.")

At most one choice set per section (a second list in the section is, by
construction, non-trailing → error).

### Parse representation

`src/blocks.rs` gains a fourth block kind. Today lists increment `nest` and emit
nothing; the walker must instead recognize a top-level all-links list, validate
it (all links, trailing), and emit:

```rust
pub enum BlockKind { Heading, Paragraph, Code, Choices }  // + Choices

pub struct ChoiceItem {
    pub label: String,
    pub target: String,   // heading text the '#' fragment named
}
// DialogBlock carries the parsed items for a Choices block (e.g. a parallel
// `choices: Option<Vec<ChoiceItem>>`, or a richer block enum — impl's call).
```

Target resolution stays **lazy/by-name** at the block level (consistent with how
`goto` resolves on use), so parsing one section doesn't require the whole file.
The editor surface (§5) is where dangling targets are reported to the author up
front; the runtime treats an unresolved target on selection as
`GotoTargetMissing` (§6), the existing reason.

## 3. The `present` redirect and `state.choice` (format)

A choice set is unconditional: every item always shows and is always selectable.
When options must be **computed** — filtered by the player's state, annotated with
a price or a roll chance, greyed out when unaffordable — the author writes them in
a code block via a new `state.next` form.

### `state.next = { t = "present", options = … }`

```lua
-- inside a fenced code block under some heading
local opts = {}
for _, lesson in ipairs(LESSONS) do
    table.insert(opts, {
        id       = "teach:" .. lesson.key,   -- token echoed back on selection
        label    = "Drill " .. lesson.display,
        target   = "Teach",                   -- heading to resume at
        note     = lesson.fee_label,          -- optional secondary text
        disabled = not can_afford(lesson),    -- optional; greyed, unselectable
    })
end
table.insert(opts, { id = "menu", label = "On second thought...", target = "Greeting" })
state.next = { t = "present", options = opts }
```

Like `goto`/`exit`, `present` is read off `state.next` after the block runs. Unlike
them it is **not a termination** — it is a **pause**: the walk stops and surfaces
`options` to the caller, then waits to be resumed (§4). Each option is
`{ id, label, target, note?, disabled? }`:

- `id` (string, required) — the token the caller hands back on selection. Ids
  must be unique within one `present`. Several options may share a `target` but
  differ in `id` (a single resume heading that reads `state.choice` to branch).
- `label` (string, required) — player-facing choice text.
- `target` (string, required) — the heading to resume at; same exact-text rule as
  a choice-set target.
- `note` (string, optional) and `disabled` (bool, optional, default false) —
  opaque presentation hints dialogmark passes through untouched. Dialogmark
  attaches no meaning to them; the consumer renders them.

`disabled` is advisory only. Dialogmark does **not** enforce it on resume — a
consumer that gates an option re-checks on selection (the host owns
authority over its own gates). Dialogmark will resume a disabled option's target
if asked; preventing that is the caller's job.

### `state.choice` (managed field)

A fifth runtime-managed field joins `idx` / `current_heading` /
`previous_heading` / `next` in [`exec::MANAGED_KEYS`](src/exec.rs):

- On a **resume** (§4), the runtime sets `state.choice` to the **id** of the
  option the player picked, before the first block of the resumed segment runs.
- It is **cleared** (set nil) after that first block, exactly as `next` is cleared
  each block — so a stale choice can't leak into a later block.
- On a fresh **open** (no prior choice), `state.choice` is nil.
- Like the other managed keys, it is filtered out of the persisted `extras`
  snapshot (`exec::snapshot_extras` already skips managed keys).

This is the mirror of the resume token a stateless handler would read. It is what
lets the example above route four `teach:*` ids plus `menu` to two headings and
have the `Teach` heading's code read `state.choice` to know which lesson was
bought.

### Precedence

Within a section a scripted `present` and a trailing choice set never both fire,
because order decides: a code block runs and sets `present` **before** the walk
reaches any later choice-set block, so the `present` pause wins and the choice-set
block is never reached. If a section has only a trailing choice set and no
redirect, the choice set is the pause. State this as a positional rule, not a
special case: **the first pause the walk reaches wins.**

## 4. The segmented walk (runtime)

The interaction consumer advances the walk **one segment at a time** — a segment
being the stretch from a resume point to the next pause (a choice point) or
termination — accumulating the narration walked along the way. This is built from,
and sits beside, the existing per-paragraph `advance`.

### 4.1 Per-walk environment isolation

**Problem.** [`exec::run_code_block`](src/exec.rs) loads each block as
`return function(state) … end` and `eval`s it in the VM's **global**
environment. Cross-block helpers therefore live in VM globals (see the
`simulate_shared_lua_state_across_blocks` test). On a VM shared by many concurrent
walks, those globals collide; and a resumed walk on a fresh-of-globals VM has lost
them.

**Contract.** Each walk runs its blocks in a **per-walk environment table**, not
in VM globals. The block chunk is loaded with that table as its `_ENV`
(`mlua::Chunk::set_environment`); the table's metatable `__index` chains to a
**base environment** the caller supplies (standard Luau stdlib plus whatever host
modules the caller injected — `require` targets, etc.). Globals a block assigns
(`LESSONS = {…}`, `function can_afford() … end`) land in the per-walk table and
are visible to later blocks **of the same walk only**; reads fall through to the
base. Two walks on one VM cannot see each other's globals, and nothing a walk
writes leaks into the shared base.

Consequence: **one shared VM can host arbitrarily many walks** (sequentially or
interleaved by a single-threaded driver) with no cross-talk. The caller no longer
needs a VM per session.

### 4.2 Prelude re-runs on resume

This is a **deliberate divergence from current `walk_with_state`**, which skips
the prelude (`prelude_ran = true`) and depends on the producing VM still holding
the prelude's globals.

With per-walk environments and JSON-only persistence between turns, the prelude's
helper definitions **cannot survive** a resume — they were functions in a previous
walk's environment, and functions don't serialize. So a resumed walk **re-runs the
prelude** into its fresh per-walk environment to re-establish helpers, **then**
seeds the saved `extras`, sets `state.choice`, and walks from the saved cursor.

This makes the prelude's existing "self-contained setup only" expectation a hard
contract: **the prelude must be idempotent and free of game-state side effects**,
because it runs once per segment, not once per conversation. (Helper definitions,
constant tables, and reads are fine; charging a fee or banking XP in the prelude
is not — those belong in walked body blocks, which run once each.) Authors who
need one-time-per-conversation setup put a guard flag in `extras`.

**Decision: generalize `walk_with_state`.** The current `walk_with_state`
semantics (skip prelude, reuse VM) and its `walk_with_state_skips_prelude` test
are **superseded**. There is one resume model, not two: `walk_with_state` is
changed to re-run the prelude into a fresh per-walk environment (and to take the
`base_env` / `resume_heading` / `choice_id` of §4.4). The
`walk_with_state_skips_prelude` test is replaced by one asserting the prelude
*does* re-run and that a side-effect-free prelude is therefore idempotent. This
is sound because per-walk isolation makes skip-prelude unworkable on a shared VM
regardless, and there is no production consumer of the old semantics yet.

### 4.3 The segment stepper

A new entry batches narration to the next pause:

```rust
/// One narration element collected while walking a segment.
pub enum Narration<'a> {
    Paragraph(&'a str),   // a paragraph block's text
    Heading(&'a str),     // a heading block's text (for consumers that render it)
}

/// Why a segment stopped.
pub enum SegmentStop {
    Present(Vec<PresentedOption>),  // a choice point — options to show, then resume
    Terminated(TerminationReason),  // EndOfDialog / Exit / errors, as today
}

/// An option surfaced to the caller at a choice point. Carries the resolved
/// target so the caller can resume without re-deriving it.
pub struct PresentedOption {
    pub id: String,
    pub label: String,
    pub target: String,        // heading text to resume at
    pub note: Option<String>,
    pub disabled: bool,
}

impl DialogWalker<'_> {
    /// Walk to the next pause, collecting narration. Headings and code blocks are
    /// processed internally (code may mutate the env / set state.next); paragraphs
    /// and heading texts are collected in order. Stops at the first Present (from a
    /// scripted `present` or a trailing choice set) or Terminated.
    pub fn advance_segment(&mut self, lua: &Lua) -> Result<(Vec<Narration<'_>>, SegmentStop), DialogError>;
}
```

`advance_segment` is the segmented analogue of `advance`. It reuses the same inner
loop (heading → update headings; code → `run_code_block`, apply `state.next`;
paragraph → collect), with two changes: it **collects** narration instead of
returning on each paragraph, and it recognizes two new pause causes —
`state.next.t == "present"` and reaching a `Choices` block — returning
`SegmentStop::Present` with the options (script-supplied, or derived from the
choice set's `ChoiceItem`s with `id = target`, `disabled = false`,
`note = None`).

The per-paragraph `advance` / `WalkStep` surface stays for streaming/visual-novel
consumers; it gains a parallel `WalkStep::Present(Vec<PresentedOption>)` pause
variant so a paragraph-stream consumer can also reach choice points. The two
share the choice-point logic in `exec`.

#### Paged walk (`advance_page`)

A second segmented surface, `advance_page`, is the **heading-boundary** analogue
of `advance_segment` — for linear, click-through dialogs where each `#` section is
its own page:

```rust
impl DialogWalker<'_> {
    /// Like `advance_segment`, but also pauses at a heading boundary: when a
    /// section falls through to the next `#` heading with no choice point and no
    /// `state.next`, returns `SegmentStop::Present` with a single synthetic
    /// option (`id = PAGE_ADVANCE_ID`, `label = "Continue"`, `target =` the next
    /// heading) so the caller resumes there next turn.
    pub fn advance_page(&mut self, lua: &Lua) -> Result<(Vec<Narration<'_>>, SegmentStop), DialogError>;
}

/// The reserved id of that synthetic "Continue" option (a public const).
pub const PAGE_ADVANCE_ID: &str;
```

It reuses the same inner stepper and pause causes; the only addition is the
heading boundary. A `goto` is **page-transparent** — the jumped-to section opens
as the page's own section, no synthetic stop — so an explicit `present` / choice
set / `goto` / `exit` / end-of-dialog behaves exactly as in `advance_segment`.
`advance_segment` (collect across headings into one segment) stays the default; a
consumer opts into `advance_page` only when it wants section-per-frame paging.
Resume is identical: the caller hands back the chosen (here synthetic) option's
`target`, and the walk continues from that heading.

### 4.4 Resume contract

A resume needs: the saved `DialogState` (cursor + extras), the **chosen option's
target**, and its **id** (for `state.choice`). The caller already holds the
presented options from the prior segment, so it passes the picked one back:

```rust
impl Dialog {
    /// Resume a segmented walk. Re-runs the prelude into a fresh per-walk env,
    /// seeds `state.extras`, sets `state.choice = choice_id`, positions the
    /// cursor at `resume_heading`, and is ready for `advance_segment`.
    pub fn resume<'a>(
        &'a self,
        lua: &Lua,
        base_env: Table,            // the caller's injected base environment
        state: DialogState,         // extras to restore (its `idx`/`next` are advisory)
        resume_heading: &str,       // the chosen option's `target`
        choice_id: Option<String>,  // the chosen option's `id` → state.choice
    ) -> Result<DialogWalker<'a>, DialogError>;
}
```

`resume_heading` is resolved with the same exact-text rule; an unresolved heading
is a `GotoTargetMissing`-shaped error. To let a caller that prefers indices map a
target itself, expose the existing `exec::resolve_goto` (or a thin
`Dialog::heading_index(name) -> Option<usize>`) publicly.

The session's stored scratch is therefore just `DialogState` (already
`Serialize`/`Deserialize`) plus, on the caller's side, the last segment's options
keyed by id (so a returned id maps to a target). Nothing else crosses a turn.

### 4.5 Open lifecycle

- **Open** = `Dialog::walk(lua, base_env, start_idx).advance_segment(lua)` → runs
  the prelude into a fresh per-walk env, walks from the start, returns the first
  segment's narration + pause.
- **Advance** = `Dialog::resume(...).advance_segment(lua)` → re-runs the prelude,
  restores extras + `state.choice`, walks the next segment.
- **Close** = a `SegmentStop::Terminated` (an `Exit` redirect, or walking off the
  end). The caller renders the trailing narration and ends the session.

`Dialog::walk` gains the `base_env: Table` parameter so the open path uses the
same isolation as resume.

## 5. Editor surface (behind the `editor` feature)

The full-trace simulator [`simulate_dialog`](src/sim.rs) is how the editor's
Validate button exercises a dialog offline. It gains parity with the new format so
a stat-gated, branchy dialog is **previewable** before it ships:

- **Choice points in the trace.** A `Choices` block and a `present` redirect each
  produce a trace entry carrying the presented options (`DialogTraceEntry` gains an
  optional `options` field, or a `"choices"` / `"present"` `kind`). The author sees
  what would be shown.
- **Interactive branch selection.** Because a choice point stops the walk, a
  one-shot linear sim can no longer cover a branchy dialog end-to-end. Either the
  editor drives the segmented API (open → render options → author clicks → resume)
  for an interactive preview, or `simulate_dialog` grows a `choices: &[String]`
  script of pre-chosen ids to replay a path deterministically (good for tests).
  Recommended: expose the segmented walk under the `editor` feature *with* tracing
  and let the editor drive it interactively; keep one-shot `simulate_dialog` for
  linear validation.
- **Mock host context is the consumer's job.** Dialogmark injects nothing
  game-specific. The editor supplies the `base_env` (its mock stats globals, a
  stubbed host module) exactly as the runtime supplies the real one — the existing
  "VM is caller-supplied" rule. Dialogmark only surfaces the choice points and
  accepts `state.choice` on resume.
- **Trailing-rule and target validation** run at validate time: a non-trailing or
  malformed choice set is a fatal warning with its line; a choice-set or `present`
  target naming no heading is reported up front (the runtime would only hit it on
  selection).

## 6. Errors and termination

New parse-time failures (surfaced as `DialogError` on the runtime fail-fast path,
and as fatal warnings with `block_idx` / `line` on the editor path):

- **Non-trailing choice set** — a top-level all-links list with content after it in
  the same section.
- **Malformed choice set** — a top-level list whose items are not all single
  `[label](#Target)` links, or an empty list, where the shape looks like a choice
  set. (A list that is plainly prose-ish — e.g. no links at all — may instead keep
  today's skip behavior; the exact line between "skip" and "reject" is an
  implementation decision, but an all-but-one-links list is the clear reject case.)

New runtime failures reuse existing `TerminationReason` variants where possible:

- A **`present` with no `options`, a non-array `options`, or an option missing
  `id`/`label`/`target`** → `ExecutionError` (same channel as a malformed
  `state.next` today; see `exec::read_next`).
- **Selecting an option whose `target` names no heading** → `GotoTargetMissing`
  (the existing reason), since resume resolves the target by exact text.
- `EndOfDialog`, `Exit`, `StepLimit`, `PreludeInvalid`, `ExecutionError` keep their
  current meanings. The `MAX_SIMULATION_STEPS` cap still bounds a single
  `advance_segment` (a goto loop inside one segment still trips it).

## 7. What does not change

- The block model for **headings, paragraphs, and fenced code** (`src/blocks.rs`),
  the four existing managed fields, the `extras` JSON round-trip
  (`exec::seed_extras` / `snapshot_extras`), exact-text goto, the `{ t, name }`
  shape of `goto`/`exit`, and the `MAX_SIMULATION_STEPS` guard.
- **Linear dialogs are unaffected.** A file with no choice sets and no `present`
  walks exactly as today (modulo the §4.2 prelude-on-resume change, which only
  affects resumed walks).
- **Dialogmark owns no VM and no game modules.** The `base_env`, host modules, and
  any mock context remain entirely the caller's, per the crate's standing rule.

## 8. Implementation notes / to verify

- **`set_environment` cost.** The per-walk env rests on loading each block chunk
  with a caller-controlled `_ENV` (`mlua 0.11.6` `Chunk::set_environment`). Confirm
  a chunk can be (pre)compiled once and re-bound to a fresh env per segment cheaply
  — e.g. compile to bytecode at parse and `Lua::load(bytecode).set_environment(env)`
  per run — rather than recompiling source each segment. If rebinding a compiled
  chunk's environment proves expensive or unsupported, fall back to recompiling per
  segment (correct, slower) and revisit. This is the one spot flagged to the
  Grindshell side as "pause and review if it needs significant changes."
- **Prelude re-run cost.** Re-running the prelude each segment is acceptable for
  rate-limited interactions but is the reason the prelude must stay pure. If a
  prelude ever needs to be heavy, the answer is to move work into guarded body
  blocks, not to cache live state across turns.
- **Choice-set lookahead.** The trailing rule needs the parser to know whether a
  top-level list is followed (within its section) by more content; `walk_dialog`
  already streams events in order, so this is a one-token-of-lookahead / deferred-
  emit change, not a second pass.
