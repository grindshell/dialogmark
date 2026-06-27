# CLAUDE.md

Onboarding doc for Claude / agents working on the **Dialogmark** crate.

## 1. Project overview

Dialogmark is a Rust library that parses Markdown files with embedded Luau code blocks and runs them as dialog trees for RPG-like games (primarily Grindshell). It owns:

- Markdown + YAML-frontmatter parsing.
- Dialog-tree structure (headings as nodes, `state.next` redirects as edges).
- Validation (frontmatter schema, prelude rules, name format).
- Simulation / execution against a caller-supplied Luau VM, with per-block tracing.

Dialogmark is intended to **replace** two existing implementations and become the single source of truth for dialog handling across Grindshell:

- [`../backend/crates/plugin`](../backend/crates/plugin) — half-finished runtime implementation. Has the VM wiring (`Plugin`, `@grindshell/fs`, `@grindshell/convert`) but no public traversal engine.
- [`../editor/crates/skill-core/src/dialog.rs`](../editor/crates/skill-core/src/dialog.rs) — finished editor-side implementation. Has full parsing, validation, simulation, and trace surface but no game-runtime hooks (fs access, etc.).

Dialogmark lifts the editor's mature surface into a standalone crate and leaves VM extensions (fs, convert, JSON, …) to the consumer.

## 2. Status

Greenfield. `src/lib.rs` is the default `cargo new --lib` stub. Cargo manifest is empty beyond `name` / `version` / `edition = "2024"`. Nothing has been ported yet.

Treat the two existing implementations as **references**, not dependencies — Dialogmark is being extracted into its own crate so neither downstream consumer leaks back into the other.

## 3. Tech stack

When porting, mirror the pinned versions used by both existing implementations so the migration is mechanical:

- `mlua` `0.11.6` with the `luau-jit` and `serialize` features. Luau (not stock Lua).
- `pulldown-cmark` `0.13.4` (or `0.13.3` — match whichever the editor uses; bump backend on integration).
- `serde` / `serde_json` `^1`.
- `thiserror` for error types (used by backend).
- `tracing` for logging (used by backend).

Edition `2024`. No async dependency in the crate itself — Dialogmark is pure / synchronous; async lives in callers.

**Cargo features**

- `editor` (off by default) — enables the editor-only surface: HTML rendering, frontmatter / prelude validation beyond what the runtime needs, full per-block tracing, and the `simulate_dialog` entry point. See §6.

## 4. Commands

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

No `mask` / workspace tooling — this crate is freestanding.

## 5. Dialog file format

Source of truth is the editor's [`dialog.rs`](../editor/crates/skill-core/src/dialog.rs); summarized here.

> **Choice-set / segmented-walk extension — implemented.**
> [CHOICES_AND_SEGMENTED_WALK.md](CHOICES_AND_SEGMENTED_WALK.md) specs the
> choice-set / `present` / segmented-walk additions that let a dialog drive a
> host's choice-per-turn interaction session. The **runtime** is built:
> `BlockKind::Choices` + `ChoiceItem`, the `state.next = { t = "present", … }`
> directive, the managed `state.choice`, per-walk environment isolation,
> prelude-on-resume, and `Dialog::walk(base_env, …)` / `Dialog::resume(…)` /
> `DialogWalker::advance_segment` (returning `Narration` + `SegmentStop`). The
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
| `name` | yes | Alphanumeric ASCII only. Validated via an `is_valid_name`-equivalent. |
| `description` | no | Free text. |
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
- Four **runtime-managed** fields on `state`:
  - `idx` — 0-based block index.
  - `current_heading` — text of the most recent heading.
  - `previous_heading` — previous heading text, or `nil`.
  - `next` — redirect spec; **cleared at the top of each block**, set by the script to redirect.
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
- `prelude_invalid` — paragraph(s) before the first heading.

## 6. Public API (target shape)

The crate exposes **two surfaces** built from the same parse:

### Runtime API (default, no features)

Lean, performance-oriented surface used during actual gameplay. **Assumes the dialog is pre-validated by the editor** — does not re-run frontmatter or prelude validation, allocates no trace entries, does no HTML rendering.

- `Dialog::parse(content: &str) -> Result<Dialog, DialogError>` — minimal parse: frontmatter (name only is structurally required) + blocks.
- A stepper / iterator on `Dialog` that takes `&Lua`, advances against the caller's Luau VM, and yields paragraphs to the caller so the game can pause between them. Exact shape (returns-paragraph-and-resumes vs. callback-driven) is an implementation detail; what's fixed is that the VM is **caller-supplied** and the runtime allocates nothing it doesn't need.

### Editor API (gated behind the `editor` Cargo feature)

Full parse + validation + simulation surface used by the editor's Validate button.

- `parse_dialog(content: &str) -> ParsedDialog` — frontmatter + body HTML + non-fatal warnings.
- `extract_blocks(content: &str) -> Vec<DialogBlock>` — top-level heading/paragraph/code blocks with 0-based `idx` and 1-based `start_line`.
- `simulate_dialog(content: &str, lua: &Lua, start_idx: usize) -> DialogSimulationResult` — full walk with per-block trace, final state snapshot, termination reason.

### Core types

`DialogFrontmatter`, `DialogBlock { idx, kind, text, start_line }`, `DialogState { idx, current_heading, previous_heading, next, extras }`, `DialogNext { t, name: Option<String> }` (where `t` is `"goto"` — `name` required — or `"exit"` — `name` absent). Editor-only (compiled under `editor`): `ParsedDialog`, `DialogTraceEntry`, `DialogSimulationResult`. All `Serialize` + `Deserialize` for cross-process use.

Errors are `thiserror`-derived enums (`DialogError`, …) — not stringly-typed, not buried inside result structs.

### VM ownership

**Dialogmark owns no `mlua::Lua`.** Every function that executes Luau (runtime stepper, editor `simulate_dialog`) takes a caller-supplied `&Lua` (or `&mut Lua` where it signals intent better). Callers wire VM extensions on their side:

- Editor: hand in an empty VM — only `state` is exposed by Dialogmark.
- Backend: hand in a sandboxed `Plugin` VM with `@grindshell/fs`, `@grindshell/convert`, etc. already loaded.

Game-specific Luau modules are **out of scope** for this crate.

## 7. Differences between the two existing implementations

When porting, prefer the **editor's** structure for most things — it's the finished one. Specific decisions to carry forward:

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

The backend's contribution to keep in mind is its **`Plugin` sandbox layer** — `@grindshell/fs` (`read_dir`, `read_to_string`, `write`, `exists`), `@grindshell/convert` (`from_json`, `to_json`, `from_position`, `to_position`). These are **out of scope for Dialogmark** but will need to be wireable on top of whatever VM hook Dialogmark exposes.

## 8. Conventions for contributors / agents

- **Runtime ≠ editor.** Gameplay dialogs are assumed pre-validated. The runtime stepper is on the hot path: no HTML rendering, no trace allocation, no re-validation beyond what's structurally needed to walk the dialog. All HTML rendering, full validation, and trace plumbing lives behind the `editor` Cargo feature (see §3 and §6).
- **Tracing is opt-in.** No `DialogTraceEntry` is built on the runtime path; trace types only compile under `editor`.
- **Dialogmark owns no `mlua::Lua`.** Every execution entry point takes a caller-supplied `&Lua`. Do not create a Luau VM inside the crate even for tests — fixture one per test.
- **Only Luau code blocks for now.** Treat every fenced code block as Luau regardless of language tag. Language-tag dispatch is future work; do not gate execution on it.
- **Goto is exact-text only.** No slug / id fallback. Renamed headings break their inbound gotos; the editor's simulator is what surfaces this to authors.
- **Step cap: `MAX_SIMULATION_STEPS = 1000`** on both surfaces. Cycle guard.
- **Errors via `thiserror`.** Define a `DialogError` enum; do not return stringly-typed errors or bury error variants inside result structs.
- **Unknown frontmatter keys are rejected.** Not warned, not silently accepted.
- Treat the two existing implementations as **read-only references** when designing the API. Do not modify them as part of porting; the cutover lands separately in `backend` and `editor`.
- When in doubt, port the editor's behavior — it's the validated one (used by the editor's Validate button) and has the better test coverage.
- Keep dialog semantics broadly compatible with the editor's so saved dialogs keep working with minimal migration. Intentional divergences from the editor (do not "fix" them back): `state.next` from prelude **redirects / terminates**; `{ t = "exit" }` is a new redirect form; frontmatter quotes are rejected; lists / nested mappings in frontmatter are fatal; unknown frontmatter keys are rejected.
- Do not bundle game-specific Luau modules (fs, convert, JSON helpers). Those belong in the consumer's VM setup.
- No async; this crate stays sync. Callers wrap it in tasks if they need to.
- Edition `2024`. Match pinned versions in §3 unless there's a concrete reason to bump.
