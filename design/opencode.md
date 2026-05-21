# Opencode Monokai Palette & Rendering Conventions

This file is the source of truth for how the **AI Activity** panel renders
during the *Update Pull Request* flow. The goal is visual parity with the
[`opencode`](https://opencode.ai) CLI/TUI running its Monokai theme so the
transcript reads as if it were emitted by opencode itself.

Two upstream sources back this document:

- `packages/opencode/src/cli/cmd/tui/context/theme/monokai.json` — the raw
  color tokens.
- `packages/opencode/src/cli/cmd/tui/routes/session/index.tsx` — the
  `TextPart`, `ReasoningPart`, `InlineTool`, `BlockTool`, and
  `AssistantMessage` components that decide how each part is laid out.

Use the regular `wisetree` palette ([`design/pallete.md`](./pallete.md)) for
the rest of the app — this file only governs the AI Activity surface.

---

## Base palette (`defs`)

| Token             | Hex       | Usage                                                  |
| ----------------- | --------- | ------------------------------------------------------ |
| `background`      | `#272822` | Main panel canvas.                                     |
| `backgroundAlt`   | `#1e1f1c` | Code-block / alternate backdrop.                       |
| `backgroundPanel` | `#3e3d32` | Elevated rows, thinking-block left border.             |
| `foreground`      | `#f8f8f2` | Primary text: assistant body, default markdown.        |
| `comment`         | `#75715e` | Muted text: thinking blocks, completed tools, dots.    |
| `red` / `pink`    | `#f92672` | Errors, markdown headings, keywords.                   |
| `orange`          | `#fd971f` | Info accent, **bold** markdown emphasis.               |
| `yellow`          | `#e6db74` | Warnings, strings, *italic* markdown emphasis.         |
| `green`           | `#a6e22e` | Success, functions, inline `code`, diff additions.     |
| `cyan` (= `blue`) | `#66d9ef` | Primary accent: borders, types, list bullets, links.   |
| `purple`          | `#ae81ff` | Secondary accent: numbers, link text, enumerations.    |

> Opencode aliases `pink = red` and `blue = cyan` in `defs`. We keep both
> names so semantic intent (`heading` ↔ pink, `link` ↔ cyan) reads cleanly.

---

## Semantic theme roles (markdown / status / surfaces)

| Role               | Color     |
| ------------------ | --------- |
| `primary`          | `#66d9ef` |
| `secondary`        | `#ae81ff` |
| `accent`           | `#a6e22e` |
| `success`          | `#a6e22e` |
| `info`             | `#fd971f` |
| `warning`          | `#e6db74` |
| `error`            | `#f92672` |
| `text`             | `#f8f8f2` |
| `textMuted`        | `#75715e` |
| `border`           | `#3e3d32` |
| `borderActive`     | `#66d9ef` |
| `borderSubtle`     | `#1e1f1c` |
| `backgroundElement`| `#3e3d32` |

Markdown bindings (mirrors `glamour`'s dark Monokai preset):

| Element                    | Color     | Style       |
| -------------------------- | --------- | ----------- |
| `markdownText`             | `#f8f8f2` | normal      |
| `markdownHeading`          | `#f92672` | bold        |
| `markdownStrong`           | `#fd971f` | bold        |
| `markdownEmph`             | `#e6db74` | italic      |
| `markdownCode` (inline)    | `#a6e22e` | normal      |
| `markdownCodeBlock`        | `#f8f8f2` | on `#1e1f1c`|
| `markdownLink`             | `#66d9ef` | underline   |
| `markdownLinkText`         | `#ae81ff` | normal      |
| `markdownListItem`         | `#66d9ef` | bullet      |
| `markdownListEnumeration`  | `#ae81ff` | enumeration |
| `markdownBlockQuote`       | `#75715e` | italic      |
| `markdownHorizontalRule`   | `#75715e` | dim         |

Syntax tokens used by `syntect`'s Monokai theme:

| Token              | Color     |
| ------------------ | --------- |
| `syntaxComment`    | `#75715e` |
| `syntaxKeyword`    | `#f92672` |
| `syntaxFunction`   | `#a6e22e` |
| `syntaxVariable`   | `#f8f8f2` |
| `syntaxString`     | `#e6db74` |
| `syntaxNumber`     | `#ae81ff` |
| `syntaxType`       | `#66d9ef` |
| `syntaxOperator`   | `#f92672` |
| `syntaxPunctuation`| `#f8f8f2` |

Diff:

| Token              | Color     | Notes                              |
| ------------------ | --------- | ---------------------------------- |
| `diffAdded`        | `#a6e22e` | `+` lines text                     |
| `diffAddedBg`      | `#1a3a1a` | `+` lines background               |
| `diffRemoved`      | `#f92672` | `-` lines text                     |
| `diffRemovedBg`    | `#3a1a1a` | `-` lines background               |
| `diffContext`      | `#75715e` | unchanged-line text                |
| `diffHunkHeader`   | `#75715e` | `@@ ... @@` lines                  |
| `diffLineNumber`   | `#9b9b95` | gutter line numbers                |

---

## Display conventions

These rules come directly from `routes/session/index.tsx`. The renderer
in `src/tui/screens/update_pr.rs::render_ai_activity_log` matches them.

### Thinking / reasoning blocks (`ReasoningPart`, `index.tsx:1492`)

- Opencode prepends `_Thinking:_ ` to the body and renders the whole block
  as markdown, with the wrapping element's `fg = textMuted` (`#75715e`).
- The `_Thinking:_` prefix is italic by markdown convention.
- No emoji (🧠 / 💭 / etc.). The label is literally `Thinking:`.
- A subtle left border in `backgroundElement` (`#3e3d32`) gutters the
  block; in the wisetree panel we approximate that with a small indent
  rather than a real left border so the AI Activity frame stays clean.
- Inline markdown inside the block (bold, italic, inline code) still
  resolves to its markdown role color even though the base color is
  muted — `**bold**` is orange, `*italic*` is yellow, `` `code` `` is
  green, etc.
- Bodies often start with a **bold mini-title** (e.g. `**Investigating
  repo setup**`); that line therefore renders bold orange via
  `markdownStrong`. We do not synthesize a separate "title" field.

### Assistant text (`TextPart`, `index.tsx:1525`)

- Plain `fg = foreground` (`#f8f8f2`), rendered as markdown.
- **No chevron, no `> ` prefix, no leading icon.** The text simply starts
  at the left margin of the body column.
- Inline markdown follows the table above.

### Tool calls (`InlineTool`, `index.tsx:1691`)

Format: `{icon} {ToolName} {short args}` — single line.

- Color: `fg = textMuted` (`#75715e`) once the call has completed, which
  is the dominant state in the transcript. Active calls flash in `text`
  until they finish.
- The icon and the tool-name keep that muted color. The argument
  fragment then runs through wisetree's hand-rolled
  `highlight_tool_args` tokenizer (deliberate divergence from opencode,
  which leaves arguments gray) so the BG_ALT code-block row reads as
  syntax-highlighted Monokai instead of dead gray text. Token map:
  - `<tag>` / `</tag>` markers → `syntaxKeyword` (`#f92672`).
  - `"…"` / `'…'` strings      → `syntaxString` (`#e6db74`).
  - `// …` trailing comments    → `syntaxComment` (`#75715e`).
  - Numbers, percentages, ≥7-char hex SHAs → `syntaxNumber` (`#ae81ff`).
  - File paths (any token containing `/`) → `syntaxType` (`#66d9ef`).
  - Keywords (`use`, `fn`, `let`, …) and arrow/comparison operators
    (`->`, `=>`, `==`, …) → `syntaxKeyword` (`#f92672`).
  - `UPPER_SNAKE_CASE` constants and `ident(`/`ident!` call/macro forms
    → `syntaxFunction` (`#a6e22e`).
  - `PascalCase` identifiers   → `syntaxType` (`#66d9ef`).
  - Everything else            → `syntaxVariable`/`foreground`.
- Tool name appears in **title case** (e.g. `Read`, `Grep`, `Skill`,
  `Bash`) — never lowercase, never with parentheses.
- The argument fragment uses a tool-specific shape (also from opencode):
  - `Bash`: just the command after `$ ` (no `Bash` word).
  - `Read`: `→ Read <path> [offset=0, limit=200]` — path then bracketed
    keyword args.
  - `Glob` / `Grep` / `List`: `✱ Grep "<pattern>" in <path>` followed by
    a paren'd match count once the call resolves.
  - `Write` / `Edit` / `Patch`: `← Edit <path>`.
  - `WebFetch` / `WebSearch`: `% WebFetch <url>` / `◈ WebSearch "<query>"`.
  - `Skill`: `→ Skill "<name>"`.
  - `Todo*` / `Batch`: `# <verb> todos`.

Icon table (matches opencode's `TOOL_RULES`):

| Tool                                  | Icon |
| ------------------------------------- | ---- |
| `bash`                                | `$`  |
| `read`                                | `→`  |
| `write` / `edit` / `patch` / `multiedit` | `←` |
| `glob` / `grep` / `list`              | `✱`  |
| `todoread` / `todowrite` / `batch`    | `#`  |
| `webfetch`                            | `%`  |
| `websearch`                           | `◈`  |
| anything else                         | `•`  |

### Tool results (`BlockTool`, `index.tsx:1645–1688`)

- Opencode hides the result body unless the tool is configured to show
  it inline. The transcript stays clean — only the *call* row is shown
  for the routine `Read`, `Glob`, `Grep`, etc.
- When a result is surfaced we render a single muted summary line
  matching the call's tool name: `→ <tool> <detail>` for success and
  `✗ <tool> <detail>` for errors. Success arrow stays in `textMuted`;
  failure cross uses `error` (`#f92672`) so it pops.
- No `[done] N tools · X.Xs · Y tokens` line per call — opencode does not
  print one and neither does the AI Activity panel.

### Errors / notices

- `error: <message>` in `error` (`#f92672`) bold.
- `warning: <message>` in `warning` (`#e6db74`) bold.
- `info: <message>` in `info` (`#fd971f`) bold.

### Session header

- `@ session <model>` in `textMuted` for `@ session ` and `text` for the
  model name. Opencode itself doesn't print a literal "session" line,
  but wisetree needs a visible header for the chosen model — we keep it
  minimal, single line, muted leading glyph.

### Footer (`AssistantMessage`, `index.tsx:1457–1481`)

- Opencode prints **one** footer at the very end of a turn:
  `▣ <mode> · <model> · <duration>`.
- Wisetree's `Summary` event surfaces total tokens streamed in a
  step_finish; we render it the same way — one muted line:
  `▣ <duration> · <tokens> tokens`. No `[done] N tools` framing,
  no bold green badge. The line color is `textMuted` throughout, with
  the leading `▣` glyph also in `textMuted`.

### Spacing rules

- A single blank line separates a Thinking block, a Text block, and a
  Tool group.
- Inside a Tool group consecutive tool calls/results are stacked with
  no blank lines.
- A blank line follows the closing footer so the next turn breathes.

All of the constants here are mirrored as Rust `Color` consts in
`src/messages.rs::colors::opencode`, so renderer code pulls from that
module instead of hard-coding RGB literals.
