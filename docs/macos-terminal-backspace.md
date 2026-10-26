# Ctrl+Backspace in Apple Terminal

On macOS 15.6 (24G84), Apple Terminal sends `7f` for both plain Backspace and
Ctrl+Backspace with its default keyboard mapping. Wisetree sees both as
unmodified Backspace. With an empty search this does nothing; with search
text it removes a character. Enhanced keyboard reporting did not distinguish
these keys in manual testing on this version.

Wisetree cannot recover a modifier that the terminal does not report. Give
Ctrl+Backspace its own terminal binding instead of changing plain Backspace.

## Set up the terminal profile

The binding is `keyMapBoundKeys["^007F"] = "\x08"`: Control plus backward-delete
sends Ctrl+H. Plain Backspace keeps sending `7f`. Wisetree accepts Ctrl+H for
the selected worktree's delete confirmation in both dashboards. This binding
also applies to other programs using that Terminal profile.

This command requires Python 3. It copies your default Terminal profile,
preserving its appearance and other bindings, adds the shortcut, and opens
the copy for import. Your original profile and default selection stay intact.

```sh
python3 - <<'PY'
import copy
import pathlib
import plistlib
import subprocess
import tempfile

preferences = pathlib.Path.home() / "Library/Preferences/com.apple.Terminal.plist"
settings = plistlib.loads(preferences.read_bytes())
original = settings["Default Window Settings"]
profile = copy.deepcopy(settings["Window Settings"][original])
profile["name"] = original + " (Ctrl-Backspace)"
profile["type"] = "Window Settings"
profile.setdefault("keyMapBoundKeys", {})["^007F"] = "\x08"
directory = pathlib.Path(tempfile.mkdtemp(prefix="wisetree-terminal-"))
output = directory / "Wisetree Ctrl-Backspace.terminal"
output.write_bytes(plistlib.dumps(profile, fmt=plistlib.FMT_BINARY))
subprocess.run(["open", str(output)], check=True)
print("Imported profile:", profile["name"])
PY
```

Test the shortcut in the imported profile's new window. To use it for new
windows, select that profile in **Terminal → Settings → Profiles** and click
**Default**. Existing tabs retain their profile; select the imported profile
through **Shell → Show Inspector → Profile** to update a tab (see
[Apple’s Inspector guide](https://support.apple.com/guide/terminal/use-the-inspector-trmlddf3251b/mac)).
To undo the change, select your original profile again and make it the default.

## Verify in Wisetree

1. Open Dashboard and select a disposable worktree. The Mother worktree is protected.
2. Press Ctrl+Backspace. The delete confirmation should open with **No** selected.
3. Press Esc to cancel. Type a search and press plain Backspace; it should only edit the search.
4. Repeat in Global Dashboard with a local worktree. Remote-only rows cannot be deleted.

On MacBooks, the key labelled **delete** is backward-delete (Backspace);
**fn+delete** is forward-delete. Wisetree also accepts Ctrl+Delete, but that
alias alone does not fix Apple Terminal's Ctrl+Backspace mapping.
