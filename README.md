# dialogmark

Create dialog trees with Markdown and inline scripting (Luau-only for now). Designed for creating content for Grindshell.

A dialog file is read top-down, so the ordering of text and code blocks matters. Exactly three Markdown structures are "blocks" that advance the dialog: **headings**, **paragraphs**, and **fenced code blocks** — plus **choice sets** (a trailing link list, see below). Everything else — blockquotes, tables, indented code, inline HTML — is skipped.

## Syntax

### Metadata

Metadata about the dialog encoded as Markdown frontmatter. Only key/value pairs where the value is always a string.

```markdown
---
name: <string>
description: <optional string>
title: <optional string>
show_headings: <optional, true>
author: <optional string>
authors: <optional string>
---
```

| Key | Required | Notes |
|---|---|---|
| `name` | yes | Alphanumeric ASCII plus underscore (`^[A-Za-z0-9_]+$`). No spaces or dashes. |
| `description` | no | Free text. |
| `title` | no | Free text. The frame title shown when the dialog drives a zone interaction. |
| `show_headings` | no | Set `show_headings: true` to render each section's `#` heading in the frame body. Off by default — the frame's panel title already carries the scene label. Exactly `true` enables it; anything else leaves it off. A code block can override it per section with `state.show_heading`. |
| `author` / `authors` | no | A single string, never a list. The two spellings are aliases — use one or the other, not both. |

Values may use the block-scalar markers `|` (keep newlines) or `>` (fold to spaces):

```markdown
---
name: Wanderer
description: >
  A long description folded
  onto a single line.
---
```

Blank lines and `# comment` lines are ignored. The following are **errors**, not warnings: quoted values (`name: "Greet"`), lists (flow or block), nested mappings, and unknown keys.

### Headings

Headings separate the dialog into sections and act as jump targets — code blocks and choice sets both redirect by heading text. Any level (`#` through `######`) works; the level itself carries no meaning.

Heading names should be unique. Lookup is by **exact text match** and takes the first match, so a duplicate heading is silently unreachable. Renaming a heading breaks every jump pointing at it — the editor's simulator is what surfaces that to authors.

By default a heading is structural and is not shown to the player; set `show_headings: true` in the frontmatter (or `state.show_heading` per section) to render it in the body.

```markdown
# Start

## Middle

# The end
```

### Text

Any Markdown paragraph after a heading. Shown to the player as narration.

```markdown
# Start

This is the start text.

This is more start text.

## End

This is end text.
```

### Code

A fenced code block. Every fenced block is executed as Luau regardless of its language tag.

````markdown
# Start

```luau
assert(state.current_heading == "Start")
state.next = {
    t = "goto",
    name = "The end"
}
```

# Skipped

This is skipped.

# The end

```luau
assert(state.current_heading == "The end")
assert(state.previous_heading == "Start")
```
````

#### State

A `state` table is passed to every code block upon execution. This table may have arbitrary keys stored in it so that different code blocks can store state. This state is cleared upon exiting the dialog.

```luau
state = {
    -- Runtime-managed fields. These are owned by the walker; assigning to them
    -- controls the dialog rather than storing data.
    idx = 0, -- The current index of the dialog, used for quickly accessing headings during runtime
    current_heading = "foo heading", -- The heading that this code block is executing under
    previous_heading = "bar heading", -- The previous heading that was executed, if any. May be nil
    choice = "ask_name", -- The id of the option the player picked to get here. Nil on a fresh start
    show_heading = true, -- Optional per-section override of the frontmatter `show_headings` default
    next = nil, -- A redirect; see below. Cleared before each block runs
}
```

Any other field you attach (`state.flag = true`, `state.counter = 1`) is a user **extra** and persists across blocks and across save points. The managed fields above do not: `next` and `show_heading` reset every block, and `choice` is set only when resuming from a player's selection.

#### Redirects

Setting `state.next` in a code block redirects the walk. It takes three forms:

```luau
-- Jump to a heading by exact text.
state.next = { t = "goto", name = "The end" }

-- End the dialog immediately.
state.next = { t = "exit" }

-- Pause and present choices to the player, resuming at the chosen target.
state.next = {
    t = "present",
    options = {
        { id = "ask_name", label = "Who are you?", target = "Who" },
        { id = "leave",    label = "Walk away",    target = "Farewell",
          note = "Ends the conversation", disabled = false },
    },
}
```

For `present`, `options` must be a non-empty array and every `id` must be unique within it. `id`, `label`, and `target` are required; `note` and `disabled` are optional presentation hints passed through to the game untouched — dialogmark does **not** enforce `disabled`, so a consumer that gates an option re-checks it on selection.

### Choice sets

A section can end with a Markdown link list instead of scripting a `present` — the declarative equivalent, for choices that don't need logic:

```markdown
# Greet

A traveller looks up as you approach.

- [Who are you?](#Who)
- [Learn an art](#Learn an art)

# Who

"Nobody in particular."
```

Each item is `[Label](#Target)`, where `Target` is the heading text to resume at — exact-text, the same rule as a `goto`, and internal spaces are preserved.

A choice set must be the **last block in its section** (the next block must be a heading, or the end of the file); anything after it would be unreachable, and that is a parse error. Every item must be a single link: a list mixing links and non-links is a parse error, while a list with no links at all is ordinary prose and is skipped.

### Prelude

Code blocks placed **before the first heading** auto-execute once at the start of every run, no matter which section the run starts at. They share the run's Luau VM, so they're the natural place for helper definitions and global setup. They re-run when a dialog is resumed, so keep them idempotent.

````markdown
---
name: Wanderer
---

```luau
function greet(who)
    return "Hello, " .. who
end
```

# Start

```luau
state.line = greet("traveller")
```
````

Setting `state.next` in the prelude does redirect or terminate the run. Paragraphs and choice sets before the first heading are an error — narration belongs under a heading.

## Termination

A run ends for one of these reasons:

| Reason | Meaning |
|---|---|
| `end_of_dialog` | Walked off the last block. |
| `exit` | A code block set `state.next = { t = "exit" }`. |
| `goto_target_missing` | A `goto` named a heading that doesn't exist. |
| `step_limit` | Visited more than 1000 blocks — almost certainly a `goto` loop. |
| `execution_error` | A Luau syntax or runtime error, or a malformed `state.next`. |
| `prelude_invalid` | A paragraph or choice set appeared before the first heading. |

## Using the crate

```rust
let dialog = dialogmark::Dialog::parse(&source)?;
let mut walker = dialog.walk(&lua, base_env, 0)?;

match walker.advance_segment(&lua)? {
    (narration, SegmentStop::Present(options)) => { /* show options, then dialog.resume(..) */ }
    (narration, SegmentStop::Terminated(reason)) => { /* done */ }
}
```

dialogmark owns no Luau VM — you supply the `&Lua` and a base environment table, which is where you install your own modules. The `editor` Cargo feature adds an authoring surface (HTML rendering, collect-all-errors validation, and `simulate_dialog` with a per-block trace) on top of the runtime one.

See [CLAUDE.md](CLAUDE.md) for the full API and [CHOICES_AND_SEGMENTED_WALK.md](CHOICES_AND_SEGMENTED_WALK.md) for the choice-set and segmented-walk design.

## License

Apache 2.0
