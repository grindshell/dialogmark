# dialogmark

Create dialog trees with Markdown and inline scripting (Luau-only for now). Designed for creating content for Grindshell.

## Syntax

Files are read top-down, so ordering of text and code blocks matters.

### Metadata

Metadata about the dialog encoded as Markdown frontmatter. Only supports key/value pairs where the value is always a string.

```markdown
---
name: <string>
description: <optional string>
author: <optional string>
authors: <optional string>
---
```

### Header

An entrypoint for the dialog. Header names must be unique. The header name is not a tangible part of the dialog and only exists to separate sections of the dialog.

Headers can be jumped to via code blocks.

```markdown
# Start

## Middle

# The end
```

### Text

Any Markdown paragraph section after a header.

```markdown
# Start

This is the start text.

This is more start text.

## End

This is end text.
```

### Code

A code block after a header.

```markdown

# Start

\`\`\`luau
assert(state.current_heading == "Start")
state.next = {
    t = "goto",
    name = "The end"
}
\`\`\`

# Skipped

This is skipped.

# The end

\`\`\`luau
assert(state.current_heading == "The end")
assert(state.previous_heading == "Start")
\`\`\`
```

#### State

A `state` table is passed to every code block upon execution. This table may have arbitrary keys stored in it so that different code blocks can store state. This state is cleared upon exiting the dialog.

```luau
state = {
    idx = 0, -- The current index of the dialog, used for quickly accessing headers during runtime
    current_heading = "foo heading", -- The heading that this code block is executing under
    previous_heading = "bar heading", -- The previous heading that was executed, if any. May be nil
    next = {
        t = "goto", -- Can also be "exit" which terminates the dialog
        name = "baz heading" -- Nil if t == "exit"
    }
}
```

## License

Apache 2.0
