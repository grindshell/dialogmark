# AGENTS.md

Onboarding doc for Claude / agents working on the **Dialogmark** crate.

## 1. Project overview

Dialogmark is a Rust library that parses Markdown files with embedded Luau code blocks and runs them as dialog trees for RPG-like games (primarily Grindshell). It owns:

- Markdown + YAML-frontmatter parsing.
- Dialog-tree structure (headings as nodes, `state.next` redirects as edges).
- Validation (frontmatter schema, prelude rules, name format).
- Simulation / execution against a caller-supplied Luau VM, with per-block tracing.

Dialogmark **replaced** two earlier implementations and is now the single source of truth for dialog handling across Grindshell:

- [`../backend/crates/plugin`](../backend/crates/plugin) — was a half-finished runtime implementation. Its dialog code is gone; what remains is the VM wiring (`Plugin`, `@grindshell/fs`, `@grindshell/convert`), and `crates/game` consumes Dialogmark directly.
- [`../editor/crates/skill-core/src/dialog.rs`](../editor/crates/skill-core/src/dialog.rs) — was the finished editor-side implementation. Now a thin re-export shim over Dialogmark plus the editor's preview-VM setup.

Dialogmark lifted the editor's mature surface into a standalone crate and leaves VM extensions (fs, convert, JSON, …) to the consumer.

## 2. Status

**Implemented, and live on both sides of the cutover.** The port is done; work here is maintenance and extension, not greenfield.

| Module | What it holds |
|---|---|
| `blocks.rs` | Markdown event walker → `DialogBlock` / `BlockKind` / `ChoiceItem`; choice-set parsing and its trailing/well-formed checks. |
| `frontmatter.rs` | Hand-rolled YAML-subset parser; fail-fast (runtime) and lenient (editor) entry points. |
| `dialog.rs` | Runtime entry point — `Dialog::parse` / `walk` / `resume` / `heading_index`. |
| `walker.rs` | The stepper — `DialogWalker` and its three advance surfaces, plus `Narration` / `WalkStep` / `SegmentStop` / `TerminationReason`. |
| `exec.rs` | Shared execution primitives: `state` table, per-walk env, `state.next` reading, Lua↔JSON, managed-field bookkeeping, `MAX_SIMULATION_STEPS`. |
| `state.rs` | `DialogState` / `DialogNext` / `PresentedOption`. |
| `error.rs` | `DialogError` / `FrontmatterError` / `ChoiceSetError`. |
| `editor.rs` (feature `editor`) | `parse_dialog` — collect-all-errors parse + HTML render. |
| `sim.rs` (feature `editor`) | `simulate_dialog` — linear walk with a per-block trace. |

Tests are inline `#[cfg(test)]` modules — 76 pass on default features, 107 with `--features editor`.

**Consumers.** Both downstream repos already depend on this crate by path, so a change here is a breaking change for both:

- [`../backend/Cargo.toml`](../backend/Cargo.toml) — default (runtime) features. `crates/game` parses dialogs into `Arc<Dialog>` at registry load and drives its interaction sessions off `walk` / `resume` / `advance_page`.
- [`../editor/crates/skill-core/Cargo.toml`](../editor/crates/skill-core/Cargo.toml) — `features = ["editor"]`. `skill-core/src/dialog.rs` re-exports the surface and hands `simulate_dialog` a VM preloaded with the editor's preview modules.

The choice-set / segmented-walk extension spec'd in [CHOICES_AND_SEGMENTED_WALK.md](CHOICES_AND_SEGMENTED_WALK.md) is built as well; see the note in §5 for what remains deferred.

## 3. Tech stack

Pinned in [`Cargo.toml`](Cargo.toml):

- `mlua` `0.11.6` with the `luau-jit` and `serde` features. Luau (not stock Lua).
- `pulldown-cmark` `0.13.4`.
- `serde` / `serde_json` `1`.
- `thiserror` `2` for error types.

No `tracing` dependency — the crate emits no logs. Diagnostics travel as return values (`DialogError`, `ParsedDialog::parse_errors`, `TerminationReason` / `terminated_reason`) and the consumer logs them.

Edition `2024`. No async dependency in the crate itself — Dialogmark is pure / synchronous; async lives in callers.

**Cargo features**

- `editor` (off by default) — compiles `editor.rs` + `sim.rs`: HTML rendering, the lenient collect-all-errors frontmatter parse, full per-block tracing, and the `simulate_dialog` entry point. See §6.

## 4. Commands

```
cargo build
cargo test                                          # 76 tests, runtime surface
cargo test --features editor                        # 107 tests, adds editor.rs + sim.rs
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

Run the tests **both ways** — `editor.rs` and `sim.rs` don't compile at all without the feature, so a default-only run silently skips a third of the suite.

No `mask` / workspace tooling — this crate is freestanding.

## 5. Dialog file format

This crate is now the source of truth for the format; the summary below tracks [`src/blocks.rs`](src/blocks.rs) / [`src/frontmatter.rs`](src/frontmatter.rs) / [`src/walker.rs`](src/walker.rs). (The format originated in the editor's `dialog.rs`, which is now a shim — see §1.)

> **Choice-set / segmented-walk extension — implemented.**
> [CHOICES_AND_SEGMENTED_WALK.md](CHOICES_AND_SEGMENTED_WALK.md) specs the
> choice-set / `present` / segmented-walk additions that let a dialog drive a
> host's choice-per-turn interaction session. The **runtime** is built:
> `BlockKind::Choices` + `ChoiceItem`, the `state.next = { t = "present", … }`
> directive, the managed `state.choice`, per-walk environment isolation,
> prelude-on-resume, and `Dialog::walk(base_env, …)` / `Dialog::resume(…)` /
> `DialogWalker::advance_segment` (returning `Narration` + `SegmentStop`).
> `DialogWalker::advance_page` is the heading-boundary analogue of
> `advance_segment` — it additionally pauses at each `#` section, emitting a
> synthetic `PAGE_ADVANCE_ID` "Continue" option, for callers that want
> section-per-frame paging of linear dialogs. The
> §5/§6 baseline below still describes the linear core. **Deferred** (§5): the
> editor's *interactive* branch preview — the linear `simulate_dialog` now stops
> at a choice point with reason `"present"` rather than walking branches.

### Frontmatter

YAML-style metadata block at the top of the file, parsed via `pulldown-cmark`'s `ENABLE_YAML_STYLE_METADATA_BLOCKS` extension. Constrained subset:

- Key-value pairs only.
- Values are strings. Only block-scalar markers `|` (keep newlines) and `>` (fold to spaces) are accepted as modifiers.
- Quoted strings (single or double) are NOT accepted.
- Lists (flow or block) and nested mappings produce **fatal warnings**; these are indicators of a malformed document.

Recognized keys:

| Key | Required | Notes |
|---|---|---|
| `name` | yes | Alphanumeric ASCII plus underscore (`^[A-Za-z0-9_]+$`). Validated via an `is_valid_name`-equivalent. |
| `description` | no | Free text. |
| `title` | no | Free text. The frame title shown when the dialog drives a zone interaction (mud.md "Dialogue-driven interactions"); the editor's `zone_interactions.luau` export lifts it into each `register_interaction` def. |
| `show_headings` | no | Boolean flag (`show_headings: true`). Whether each section's `#` heading renders in the frame body. **Off by default** — the frame's panel title carries the scene label, so an in-body heading is redundant. Consumers (the runtime driver / editor preview) read it; the walker still emits `Narration::Heading` regardless. |
| `author` / `authors` | no | String or multiline string. **Never a list.** Aliases collapse to one field; duplicates warn. |

### Body

Markdown with three structural elements that matter for the dialog tree:

- **Headings** (any level, 1–6) — node boundaries.
- **Paragraphs** — narration shown to the player.
- **Fenced code blocks** — Luau, lifted into `function(state) … end` and threaded against a persistent state table.

Lists, blockquotes, tables, indented code, inline HTML are **skipped** during block extraction (depth > 0 in the event walker). They render in HTML preview but don't contribute to the dialog flow.

### Control flow

- A single Luau VM persists across the whole run.
- A single `state` table threads across all blocks.
- **Runtime-managed** fields on `state` (owned by the walker, filtered out of `extras`):
  - `idx` — 0-based block index.
  - `current_heading` — text of the most recent heading.
  - `previous_heading` — previous heading text, or `nil`.
  - `next` — redirect spec; **cleared at the top of each block**, set by the script to redirect.
  - `choice` — the id of the option the player picked to reach this segment, set by [`Dialog::resume`](src/dialog.rs) after the prelude runs (so prelude blocks don't consume it). `nil` on a fresh walk.
  - `show_heading` — optional per-segment override (`true`/`false`) of whether this frame's `#` heading renders in the body; surfaced on `DialogState.show_heading` for the consumer to resolve over the frontmatter `show_headings` default. Runtime-managed like `next` (never seeded from a resumed snapshot), so it **resets each segment** — a script sets it per section.
- Any other field the script attaches (`state.flag = true`, `state.counter = 1`, …) is treated as a **user "extra"** and persists across blocks. Extras are surfaced back to callers as a `BTreeMap<String, serde_json::Value>`.
- Redirect form: `state.next = { t = "goto", name = "<HeadingText>" }`. Heading lookup is by exact text match. Missing target → terminate with reason `goto_target_missing`.
- Exit form: `state.next = { t = "exit" }`. Terminates immediately with reason `exit`.

### Prelude

Code blocks **before the first heading** auto-execute once at the start of every run, regardless of where the run starts. They share the walk's Luau VM, so they're the natural place for helper definitions and global setup. `state.next` set during the prelude **does redirect / terminate**.

**Paragraphs before the first heading are a validation error.** Simulation returns `terminated_reason = "prelude_invalid"` before any Luau executes. Narration belongs under a heading.

### Termination reasons

- `end_of_dialog` — walked off the last block.
- `exit` — `state.next` was set to `{ t = "exit" }`.
- `goto_target_missing` — `state.next.name` didn't match any heading.
- `step_limit` — exceeded `MAX_SIMULATION_STEPS` (1000). Cycle guard.
- `execution_error` — Luau syntax or runtime error.
- `prelude_invalid` — paragraph(s) (or a choice set) before the first heading. Enforced on **both** surfaces: the runtime walker terminates with `TerminationReason::PreludeInvalid { block_idx, line }` before running any Luau, and `simulate_dialog` returns the `"prelude_invalid"` string.
- `present` — **`simulate_dialog` only.** The linear walk reached a choice point (a `present` directive or a trailing choice set) and stopped rather than branching. The runtime surfaces this as `SegmentStop::Present` / `WalkStep::Present`, not a termination.

The runtime's [`TerminationReason`](src/walker.rs) enum is the typed form of the first six; `simulate_dialog` flattens it to the strings above, which is what the editor frontend consumes.

## 6. Public API

Everything below is re-exported from [`src/lib.rs`](src/lib.rs). The crate exposes **two surfaces** built from the same parse.

### Runtime API (default, no features)

Lean, performance-oriented surface used during actual gameplay. **Assumes the dialog is pre-validated by the editor** — no HTML rendering, no trace allocation, no collect-all-errors diagnostics. (It *does* still enforce the prelude rule and fail fast on frontmatter / choice-set faults; those are structural, not diagnostics.)

- `Dialog::parse(content: &str) -> Result<Dialog, DialogError>` — one-pass parse: frontmatter + blocks, erroring on the first frontmatter fault or (by source line) the earliest choice-set fault. Yields `Dialog { frontmatter, blocks }`.
- `Dialog::walk(&self, lua: &Lua, base_env: Table, start_idx: usize) -> Result<DialogWalker, DialogError>` — start a fresh walk. Blocks run in a per-walk environment chained to `base_env`, so many walks share one VM without colliding. The prelude runs lazily on the first advance.
- `Dialog::resume(&self, lua: &Lua, base_env: Table, state: DialogState, resume_heading: &str, choice_id: Option<String>) -> Result<DialogWalker, DialogError>` — resume from a saved snapshot at a chosen option's target: re-runs the prelude into a fresh env, restores `extras`, sets `state.choice`.
- `Dialog::heading_index(&self, name: &str) -> Option<usize>` — exact-text heading lookup, for callers that prefer indices.
- `extract_blocks(content: &str) -> Vec<DialogBlock>` — standalone block extraction (no frontmatter), with 0-based `idx` and 1-based `start_line`. Available on **both** surfaces.

`DialogWalker` has three advance surfaces over one shared block stepper, all borrowing (never owning) the `&Lua`:

- `advance(&mut self, lua: &Lua) -> Result<WalkStep, DialogError>` — run to the next **paragraph**, choice point, or termination. The streaming / visual-novel surface. Headings are internal here.
- `advance_segment(&mut self, lua: &Lua) -> Result<(Vec<Narration>, SegmentStop), DialogError>` — run to the next choice point or termination, **collecting** the narration walked along the way. The choice-per-turn surface.
- `advance_page(&mut self, lua: &Lua) -> Result<(Vec<Narration>, SegmentStop), DialogError>` — as `advance_segment`, but also pausing at each `#` heading boundary, emitting a synthetic one-option `SegmentStop::Present` with id `PAGE_ADVANCE_ID` ("Continue"). Section-per-frame paging of linear dialogs. A `goto` is page-transparent.

Plus `snapshot(&self) -> DialogState` (save points — `idx`, headings, `extras`; the managed `next` / `choice` are deliberately not surfaced) and `cursor(&self) -> usize`.

All three advance methods are idempotent once terminated. `Err` is reserved for VM plumbing failures Dialogmark can't recover from — script faults come back as a `TerminationReason`, not an `Err`.

### Editor API (gated behind the `editor` Cargo feature)

Full parse + validation + simulation surface used by the editor's Validate button.

- `parse_dialog(content: &str) -> ParsedDialog` — frontmatter + body HTML + **all** frontmatter errors at once (`parse_errors`), rather than failing on the first. The dialog is returned regardless so the UI can show the file alongside its problems.
- `parse_frontmatter_lenient(yaml: &str) -> (DialogFrontmatter, Vec<FrontmatterError>)` — the collect-all-errors frontmatter parse on its own.
- `simulate_dialog(content: &str, lua: &Lua, start_idx: usize) -> DialogSimulationResult` — linear walk with a per-block trace, final state snapshot, and a `terminated_reason` string. Never returns `Err`; every failure materializes in the result. Parses via `extract_blocks`, so authors can validate files with broken frontmatter.

### Core types

Runtime surface: `Dialog { frontmatter, blocks }`, `DialogFrontmatter { name, description, title, show_headings, author }`, `DialogBlock { idx, kind, text, start_line, choices }`, `BlockKind { Heading, Paragraph, Code, Choices }`, `ChoiceItem { label, target }`, `DialogState { idx, current_heading, previous_heading, next, show_heading, extras }`, `DialogNext { t, name: Option<String> }` (`t` is `"goto"` — `name` required — `"exit"`, or `"present"` — `name` absent for both), `PresentedOption { id, label, target, note, disabled }`, `DialogWalker`, `Narration`, `WalkStep`, `SegmentStop`, `TerminationReason`, `PAGE_ADVANCE_ID`.

Editor-only (compiled under `editor`): `ParsedDialog`, `DialogTraceEntry`, `DialogSimulationResult`.

The **data** types are `Serialize` + `Deserialize` for cross-process use — frontmatter, blocks, state, options, trace, results, and the `FrontmatterError` / `ChoiceSetError` leaf diagnostics. The **engine** types are not: `Dialog`, `DialogWalker`, `Narration` / `WalkStep` / `SegmentStop` / `TerminationReason`, and `DialogError` (it wraps `mlua` failures as strings and is meant to be handled, not shipped).

Errors are `thiserror`-derived enums — `DialogError` (the wrapper, with `Frontmatter` / `ChoiceSet` / `StateInit` / `LuaInternal` variants), `FrontmatterError`, `ChoiceSetError`. Not stringly-typed, not buried inside result structs. The one deliberate exception is `DialogSimulationResult::terminated_reason`, a string because the editor frontend already consumes those tokens.

### VM ownership

**Dialogmark owns no `mlua::Lua`.** Every function that executes Luau (runtime stepper, editor `simulate_dialog`) takes a caller-supplied `&Lua` (or `&mut Lua` where it signals intent better). Callers wire VM extensions on their side:

- Editor: hand in an empty VM — only `state` is exposed by Dialogmark.
- Backend: hand in a sandboxed `Plugin` VM with `@grindshell/fs`, `@grindshell/convert`, etc. already loaded.

Game-specific Luau modules are **out of scope** for this crate.

## 7. Differences between the two predecessor implementations

Historical — the port took the **editor's** structure for most things, since it was the finished one. Kept here because it records *why* the current surface looks the way it does, and because saved dialogs still have to stay compatible with the editor column:

| Topic | Editor (preferred) | Backend (legacy) |
|---|---|---|
| Parse entry point | `parse_dialog(&str)` returns frontmatter + HTML + warnings | `Dialog::parse(vm, path)` / `parse_str(vm, input)` |
| Block extraction | `extract_blocks(&str) -> Vec<DialogBlock>` — pure | Inlined into parse; no public extraction |
| Simulation | `simulate_dialog(&str, start_idx) -> DialogSimulationResult` — full trace | None public — `Dialog::get` is `todo!()` |
| `state.next` shape | `{ t = "goto", name = <string> }`; `{ t = "exit" }`; managed field cleared each block | Same shape, but no enforcement / clearing |
| Prelude rules | Code-only; paragraphs before first heading rejected as `prelude_invalid`; `state.next` from prelude **redirects / terminates** the walk (Dialogmark divergence — editor records but ignores it) | Single optional top-of-file code block auto-executed |
| YAML parser | Hand-rolled, supports `\|` and `>`; lists/nested mappings fatal; quotes rejected (Dialogmark tightens both — editor allowed quotes and warned on lists/nested) | Hand-rolled, similar but slightly different edge cases |
| Code block detection | Any fenced block treated as Luau (TODO acknowledged) | Any fenced block treated as Luau (TODO acknowledged) |
| VM scope | Fresh `Lua` per simulation, no game modules | Long-lived `Plugin` with `@grindshell/fs`, `@grindshell/convert` |
| Step cap | `MAX_SIMULATION_STEPS = 1000` | None |

The backend's contribution was its **`Plugin` sandbox layer** — `@grindshell/fs` (`read_dir`, `read_to_string`, `write`, `exists`), `@grindshell/convert` (`from_json`, `to_json`, `from_position`, `to_position`). These stayed **out of scope for Dialogmark**; the `base_env` parameter on `Dialog::walk` / `Dialog::resume` is the hook they wire onto.

## 8. Conventions for contributors / agents

- **Runtime ≠ editor.** Gameplay dialogs are assumed pre-validated. The runtime stepper is on the hot path: no HTML rendering, no trace allocation, no re-validation beyond what's structurally needed to walk the dialog. All HTML rendering, full validation, and trace plumbing lives behind the `editor` Cargo feature (see §3 and §6).
- **Tracing is opt-in.** No `DialogTraceEntry` is built on the runtime path; trace types only compile under `editor`.
- **Dialogmark owns no `mlua::Lua`.** Every execution entry point takes a caller-supplied `&Lua`. Do not create a Luau VM inside the crate even for tests — fixture one per test.
- **Only Luau code blocks for now.** Treat every fenced code block as Luau regardless of language tag. Language-tag dispatch is future work; do not gate execution on it.
- **Goto is exact-text only.** No slug / id fallback. Renamed headings break their inbound gotos; the editor's simulator is what surfaces this to authors.
- **Step cap: `MAX_SIMULATION_STEPS = 1000`** on both surfaces. Cycle guard.
- **Errors via `thiserror`.** Extend the existing `DialogError` / `FrontmatterError` / `ChoiceSetError` enums; do not return stringly-typed errors or bury error variants inside result structs. (`terminated_reason` is the one grandfathered string — see §6.)
- **Unknown frontmatter keys are rejected.** Not warned, not silently accepted.
- **Both consumers are live (§2).** Any change to the public surface breaks `backend` and `editor` simultaneously — check both before changing a signature, and land the crate change together with their updates.
- When in doubt, follow the editor's original behavior — it's the validated one (used by the editor's Validate button) and had the better test coverage.
- Keep dialog semantics broadly compatible with the editor's so saved dialogs keep working. Intentional divergences from the editor (do not "fix" them back): `state.next` from prelude **redirects / terminates**; `{ t = "exit" }` is a new redirect form; frontmatter quotes are rejected; lists / nested mappings in frontmatter are fatal; unknown frontmatter keys are rejected.
- Do not bundle game-specific Luau modules (fs, convert, JSON helpers). Those belong in the consumer's VM setup.
- No async; this crate stays sync. Callers wrap it in tasks if they need to.
- Edition `2024`. Match pinned versions in §3 unless there's a concrete reason to bump.
