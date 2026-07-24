# Vendored `vt100` 0.15.2 — wisetree fork

This is an unmodified copy of [`vt100`](https://github.com/doy/vt100-rust) 0.15.2
from crates.io, with two surgical patches in `src/grid.rs`. It is wired in via
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

Inline-rendering AI harnesses (codex, claude) commit their transcript by
scrolling *within a top-anchored DECSTBM scroll region* (`\x1b[1;Nr`) that
reserves the bottom composer — they never enter the alternate screen, and they
rely on the host terminal capturing scrolled-off lines into scrollback. Stock
vt100 only pushes a scrolled-off row into scrollback when **no** scroll region
is active, so every committed line under an active region is discarded. The
embedded PTY panel could therefore never scroll back through codex/claude
history (`scrollback_len` stayed 0), unlike a real terminal.

## Patches (`src/grid.rs`)

1. **`scroll_up` — capture top-anchored regions.** The capture guard changed
   from `!self.scroll_region_active()` to `self.scroll_top == 0`. A region whose
   top margin is row 1 scrolls its top row into scrollback exactly like xterm;
   bottom-anchored / mid-screen regions still don't. Safe for alt-screen apps
   (opencode): the alternate grid is built with `scrollback_len == 0`, so the
   `scrollback_len > 0` half of the guard keeps capture off there.

2. **`visible_rows` — saturate the offset subtraction.** Both
   `scrollback_len - offset` and `rows_len - offset` are now `saturating_sub`.
   Once scrollback fills and the child later clears it (`\x1b[3J`) while the user
   is scrolled back, `offset` can exceed the live buffer length and the raw
   subtraction panics ("attempt to subtract with overflow").

Keep this file and the patch markers in `src/grid.rs` in sync if the pin is ever
bumped. See the wisetree memory note `reference_vt100_region_scrollback`.
