# Dashboard shortcuts

Press **Ctrl+X** to open the delete confirmation for the selected local worktree
in Dashboard or Global Dashboard. The shortcut works with an empty search or
while searching, targets the highlighted visible row, and leaves the search
query unchanged.

The confirmation defaults to **No**. Pressing Enter immediately cancels the
request. Select **Yes** (or **Force Delete** for uncommitted changes) explicitly
to proceed. Existing checks and additional confirmations for unmerged branches
still apply. The Mother worktree is protected, and remote-only rows cannot be
deleted.

Backspace, Delete, Ctrl+H, Ctrl+Backspace and Ctrl+Delete retain their search
editing behavior rather than requesting deletion. On Apple Terminal,
Ctrl+Backspace may arrive as ordinary Backspace and edit one character.
Ctrl+W and Alt+Backspace delete the previous word.

Ctrl+X uses normal terminal keyboard events, including Apple Terminal's
Control-X byte (`18` in hexadecimal). No macOS keyboard changes or custom
terminal profiles are needed. The previous terminal-profile workaround is
unnecessary; use your normal terminal profile.

To verify: search for a disposable worktree, press Ctrl+X, and press Enter
without moving the confirmation selection. The worktree should remain, the
dashboard should reopen with the same search, and Backspace should still edit
that search. Repeat in Global Dashboard with a local worktree row.
