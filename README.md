# 🌳 Wisetree

[![npm version](https://badge.fury.io/js/wisetree.svg)](https://www.npmjs.com/package/wisetree)
[![license](https://img.shields.io/npm/l/wisetree.svg)](https://www.npmjs.com/package/wisetree)

An interactive CLI for creating and managing Git worktrees, with an easy-to-use terminal interface. Wisetree is distributed as a single static binary.

## Features

- **Quick Commands**: Jump directly to specific actions via command line
- **Smart Configuration**: Project-specific and global configuration support
- **File Management**: Automatically copy configuration files to new worktrees
- **Post-Create Actions**: Run custom commands after worktree creation
- **Shell Integration**: Optional zsh/bash wrapper that lets you `cd` into a worktree directly from the picker
- **Single Static Binary**: No Node runtime required — installable via npm, Homebrew, or a curl shell script

## Installation

### npm (recommended)

```bash
npm install -g wisetree
```

The `wisetree` package is a thin shim that pulls in a precompiled binary for your platform via `optionalDependencies`. Supported platforms: macOS (arm64 + x64), Linux (x64 + arm64 gnu), Windows (x64 msvc).

### Homebrew (macOS)

```bash
brew tap victorcorcos/tap
brew install wisetree
```

### Shell installer (Linux / macOS)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/victorcorcos/wisetree/releases/latest/download/wisetree-installer.sh | sh
```

### Cargo (build from source)

```bash
cargo install wisetree
```

## Quick Start

Run Wisetree in any Git repository:

```bash
wisetree
```

This opens an interactive menu where you can:

- Create new worktrees
- List existing worktrees
- Delete worktrees
- Configure settings
- Install shell integration

## Commands

### Interactive Menu (Default)

```bash
wisetree
```

Opens the main menu with all available options.

### Direct Commands

```bash
wisetree create    # Go directly to worktree creation
wisetree list      # List all worktrees
wisetree delete    # Go directly to worktree deletion
wisetree settings  # Open settings menu
```

### Options

```bash
wisetree --help     # Show help information
wisetree --version  # Show version number
wisetree -m create  # Set initial mode
```

### Non-Interactive (Scriptable) CLI

Pass flags to skip the interactive prompts entirely. The three core concepts map directly to flags:

| Flag                    | Description                                     |
| ----------------------- | ----------------------------------------------- |
| `-n, --name <name>`     | Worktree directory name                         |
| `-s, --source <branch>` | Source branch to create from                    |
| `-b, --branch <branch>` | New branch name (defaults to `-n` when omitted) |

```bash
# Create a worktree — new branch defaults to the worktree name
wisetree create -n my-feature -s main

# Create a worktree with an explicit branch name different from the directory
wisetree create -n ticket-3121-backend -s main -b feature/ticket-3121

# Create multiple sibling worktrees from the same source branch
# (each gets its own branch, so there's no checkout conflict)
wisetree create -n digit3121-backend  -s main -b feat/digit3121-backend
wisetree create -n digit3121-frontend -s main -b feat/digit3121-frontend

# List worktrees as JSON
wisetree list --json

# Delete a worktree by name
wisetree delete -n my-feature

# Force-delete a worktree by path
wisetree delete -p /path/to/worktree -f
```

Successful `create` output:

```
/path/to/worktree
  source: main
  branch: my-feature
```

## Shell Integration

Wisetree ships with a tiny shell wrapper that lets you `cd` directly into the worktree you pick from the list view. Without it, the binary can't change your shell's current directory (no process can change its parent's `cwd`); the wrapper bridges that gap by running Wisetree, capturing the chosen path, and `cd`-ing for you.

Install it from the menu (`Setup Shell Integration`) or the wizard:

```bash
wisetree   # then pick "Setup Shell Integration"
```

The wizard appends a marked block to `~/.zshrc` or `~/.bashrc`:

```sh
# Wisetree setup: added on YYYY-MM-DD
# (tab completions + a wrapper function)
wisetree() {
  if [ $# -eq 0 ]; then
    local dir=$(FORCE_COLOR=3 command wisetree --from-wrapper)
    if [ -n "$dir" ]; then
      builtin cd "$dir" && echo "Wisetree: Navigated to $(pwd)"
    fi
  else
    command wisetree "$@"
  fi
}
# End Wisetree setup
```

Re-running the wizard replaces the existing block in place, so you can safely upgrade.

> **macOS bash users**: bash login shells on macOS read `~/.bash_profile` rather than `~/.bashrc`. If the integration doesn't activate in new terminals, add `[ -f ~/.bashrc ] && source ~/.bashrc` to your `~/.bash_profile`. Wisetree writes to `~/.bashrc` for cross-platform consistency.

## Configuration

Wisetree looks for configuration files in this order:

1. `.wisetree.json` in your repo's root (project-specific)
2. `~/.wisetree/settings.json` (global configuration)

### Configuration Options

Create a `.wisetree.json` file in your project root or configure global settings:

```json
{
  "$schema": "https://raw.githubusercontent.com/victorcorcos/wisetree/main/schema.json",
  "worktreeCopyPatterns": [".env*", ".vscode/**"],
  "worktreeCopyIgnores": ["**/node_modules/**", "**/dist/**", "**/.git/**"],
  "worktreePathTemplate": "$BASE_PATH.worktree",
  "postCreateCmd": ["npm install", "npm run db:generate"],
  "terminalCommand": "code .",
  "deleteBranchWithWorktree": true
}
```

#### Configuration Fields

- **`worktreeCopyPatterns`**: Files/directories to copy to new worktrees (supports glob patterns)
  - Default: `[".env*", ".vscode/**"]`
  - Examples: `["*.json", "config/**", ".env.local"]`

- **`worktreeCopyIgnores`**: Files/directories to exclude when copying (supports glob patterns)
  - Default: `["**/node_modules/**", "**/dist/**", "**/.git/**", "**/Thumbs.db", "**/.DS_Store"]`

- **`worktreePathTemplate`**: Template for worktree directory names
  - Default: `"$BASE_PATH.worktree"`
  - Variables: `$BASE_PATH`, `$WORKTREE_PATH`, `$BRANCH_NAME`, `$SOURCE_BRANCH`
  - Examples: `"worktrees/$BRANCH_NAME"`, `"$BASE_PATH-branches/$BRANCH_NAME"`

- **`postCreateCmd`**: Commands to run after creating a worktree. Runs in the new worktree directory.
  - Default: `[]`
  - Examples: `["npm install"]`, `["pnpm install", "pnpm build"]`
  - Variables supported in commands: `$BASE_PATH`, `$WORKTREE_PATH`, `$BRANCH_NAME`, `$SOURCE_BRANCH`

- **`terminalCommand`**: Command to open terminal/editor in the new worktree. Runs in the new worktree directory.
  - Default: `""`
  - Examples: `"code ."`, `"cursor ."`, `"zed ."`

- **`deleteBranchWithWorktree`**: Whether to also delete the associated git branch when deleting a worktree
  - Default: `false`
  - When enabled, deleting a worktree will also delete its branch (with safety checks)
  - Shows warnings for branches with unpushed commits or uncommitted changes

### Template Variables

Available in `worktreePathTemplate`, `postCreateCmd`, and `terminalCommand`:

- `$BASE_PATH`: Base name of your repository
- `$WORKTREE_PATH`: Full path to the new worktree
- `$BRANCH_NAME`: Name of the new branch
- `$SOURCE_BRANCH`: Name of the source branch

## Usage Examples

### Basic Workflow

1. **Navigate to your Git repository**

   ```bash
   cd my-project
   ```

2. **Start Wisetree**

   ```bash
   wisetree
   ```

3. **Create a worktree**
   - Select "Create new worktree"
   - Enter directory name (e.g., `feature-auth`)
   - Choose source branch (e.g., `main`)
   - Enter new branch name (e.g., `feature/authentication`)
   - Confirm creation

### Project-Specific Configuration

Create `.wisetree.json` in your project:

```json
{
  "$schema": "https://raw.githubusercontent.com/victorcorcos/wisetree/main/schema.json",
  "worktreeCopyPatterns": [
    ".env.local",
    ".vscode/**",
    "package.json",
    "tsconfig.json"
  ],
  "worktreePathTemplate": "worktrees/$BRANCH_NAME",
  "postCreateCmd": ["npm install", "npm run db:populate"],
  "terminalCommand": "code .",
  "deleteBranchWithWorktree": true
}
```

## Requirements

- Git installed and available in `PATH`
- Operating system: macOS, Linux, or Windows
- A terminal that speaks ANSI escapes (most modern terminals)

## License

MIT
