# Opencode Monokai Palette

This palette mirrors the Monokai theme shipped with the
[`opencode`](https://opencode.ai) CLI/TUI
(`packages/opencode/src/cli/cmd/tui/context/theme/monokai.json`). It is the
source of truth for every color rendered inside wisetree's **AI Activity**
panel during the *Update Pull Request* flow so the transcript looks identical
to what opencode itself prints in its TUI.

Use the regular `wisetree` palette
([`design/pallete.md`](./pallete.md)) for the rest of the app — this file only
governs the AI Activity surface.

---

## Base palette (`defs`)

The raw color tokens opencode composes the theme from. All values are sRGB
hex strings — translate to `Color::Rgb(r, g, b)` at the ratatui call site.

| Token             | Hex       | Usage                                                    |
| ----------------- | --------- | -------------------------------------------------------- |
| `background`      | `#272822` | Main panel background (the canvas the transcript sits on). |
| `backgroundAlt`   | `#1e1f1c` | Subtle / secondary panel background, code-block backdrop. |
| `backgroundPanel` | `#3e3d32` | Elevated panel background (selected rows, status strips). |
| `foreground`      | `#f8f8f2` | Primary text — assistant body, default code-block text.   |
| `comment`         | `#75715e` | Muted text — thinking blocks, hunk headers, list dots.    |
| `red` / `pink`    | `#f92672` | Errors, markdown headings, syntax keywords/operators.     |
| `orange`          | `#fd971f` | Info accents, **bold** markdown emphasis.                 |
| `lightOrange`     | `#e69f66` | Soft orange highlight (rare).                             |
| `yellow`          | `#e6db74` | Warnings, *italic* markdown emphasis, string literals.    |
| `green`           | `#a6e22e` | Success, function names, inline `code`, diff additions.   |
| `cyan` (= `blue`) | `#66d9ef` | Primary accent — borders, types, list bullets, links.     |
| `purple`          | `#ae81ff` | Secondary accent — numbers, link text, enumerations.      |

> Opencode aliases `pink = red` and `blue = cyan` in its `defs` block. We keep
> the same aliases so semantic intent (`heading` ↔ pink, `link` ↔ cyan) reads
> cleanly in the renderer.

---

## Semantic theme roles

These are the keys the opencode TUI binds to every surface; wisetree's AI
Activity panel re-uses them verbatim.

### Surfaces

| Role               | Color     | Notes                                                |
| ------------------ | --------- | ---------------------------------------------------- |
| `background`       | `#272822` | Panel canvas.                                        |
| `backgroundPanel`  | `#1e1f1c` | Header / status strip backdrop.                      |
| `backgroundElement`| `#3e3d32` | Selected rows, code-block backdrop, focused element. |
| `border`           | `#3e3d32` | Default panel border.                                |
| `borderActive`     | `#66d9ef` | Active / focused border (cyan).                      |
| `borderSubtle`     | `#1e1f1c` | Inner dividers.                                      |

### Status

| Role         | Color     |
| ------------ | --------- |
| `primary`    | `#66d9ef` |
| `secondary`  | `#ae81ff` |
| `accent`     | `#a6e22e` |
| `success`    | `#a6e22e` |
| `info`       | `#fd971f` |
| `warning`    | `#e6db74` |
| `error`      | `#f92672` |
| `text`       | `#f8f8f2` |
| `textMuted`  | `#75715e` |

### Markdown

Opencode renders assistant text with these mappings (matches `glamour`'s
dark Monokai preset):

| Element                | Color     | Style                          |
| ---------------------- | --------- | ------------------------------ |
| `markdownText`         | `#f8f8f2` | normal                         |
| `markdownHeading`      | `#f92672` | bold                           |
| `markdownStrong`       | `#fd971f` | bold                           |
| `markdownEmph`         | `#e6db74` | italic                         |
| `markdownCode` (inline)| `#a6e22e` | normal                         |
| `markdownCodeBlock`    | `#f8f8f2` | normal, on `#1e1f1c` backdrop  |
| `markdownLink`         | `#66d9ef` | underline                      |
| `markdownLinkText`     | `#ae81ff` | normal                         |
| `markdownImage`        | `#66d9ef` | normal                         |
| `markdownImageText`    | `#ae81ff` | normal                         |
| `markdownListItem`     | `#66d9ef` | bullet glyph color             |
| `markdownListEnumeration` | `#ae81ff` | enumeration glyph color     |
| `markdownBlockQuote`   | `#75715e` | italic                         |
| `markdownHorizontalRule`| `#75715e`| dim                            |

### Code syntax (used by `syntect` Monokai theme)

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

### Diff

| Token                  | Color     | Notes                                    |
| ---------------------- | --------- | ---------------------------------------- |
| `diffAdded`            | `#a6e22e` | `+` lines text                           |
| `diffAddedBg`          | `#1a3a1a` | `+` lines background                     |
| `diffRemoved`          | `#f92672` | `-` lines text                           |
| `diffRemovedBg`        | `#3a1a1a` | `-` lines background                     |
| `diffContext`          | `#75715e` | unchanged-line text                      |
| `diffContextBg`        | `#1e1f1c` | unchanged-line background                |
| `diffHunkHeader`       | `#75715e` | `@@ ... @@` lines                        |
| `diffLineNumber`       | `#9b9b95` | gutter line numbers                      |
| `diffAddedLineNumberBg`| `#1a3a1a` | gutter background for added lines        |
| `diffRemovedLineNumberBg`| `#3a1a1a`| gutter background for removed lines     |

---

## Display conventions

Beyond raw colors, the AI Activity panel reproduces opencode's terminal
formatting so the output reads as a familiar opencode transcript:

- **Thinking blocks** — prefix `Thinking:` in italic muted comment color, body
  italic + dim foreground.
- **Assistant text** — markdown is rendered with the table above; trailing
  whitespace stripped, no truncation cap so multi-line answers flow.
- **Tool calls** — single line `* <icon> <tool>(<short args>)` with the `*`
  bullet in cyan, the tool name in green, arguments in muted comment.
- **Tool results** — `→ <tool> <ok|error> <detail>`, the arrow green on
  success and pink on error.
- **Tool icons** — match opencode's `TOOL_RULES`: `→` read, `←` write/edit,
  `

 bash, `✱` glob/grep, `#` batch/todo, `%` webfetch, `◈` websearch.
- **Code fences** — rendered with `syntect`'s bundled Monokai theme so spans
  align with `syntax*` colors above; backdrop is `backgroundAlt` (`#1e1f1c`).
- **Diff output** — additions on `diffAddedBg`, removals on `diffRemovedBg`,
  hunk headers in comment grey, line numbers in `#9b9b95`.
- **Summary line** — `[done] N tools · X.Xs · Y tokens`, `done` rendered bold
  green; the rest in muted comment color.

All of the constants here are mirrored as Rust `Color` consts in
`src/messages.rs::colors::opencode` and `::colors::monokai`, so renderer code
should pull from those modules instead of hard-coding RGB literals.
