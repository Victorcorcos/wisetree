# 🔗 Shared Dependency Cache (Symlink Patterns) — Implementation Plan Prompt

You are implementing a major productivity feature in the `wisetree` Rust codebase: **shared dependency caches via symlinks**, so heavyweight directories like `node_modules/`, `target/`, `vendor/`, and `.venv/` are not re-installed for every new worktree. Instead, they are linked from a per-repo cache directory and shared across worktrees.

Read this prompt as a self-contained brief. Match the project conventions described below — do not invent new ones. When unsure, mirror what `FileService::copy_files` and `WorktreeService::create_worktree` already do.

---

## 1. Goal & Scope

Today, `wisetree` *copies* files into a new worktree (`worktreeCopyPatterns`) and runs `postCreateCmd` to install dependencies fresh. For a typical Node/Rust/Ruby repo, that means downloading hundreds of MB and burning minutes per worktree.

This feature introduces a third operation between copy and post-create: **link**. A new config field `worktreeLinkPatterns` describes directories whose contents should be shared across worktrees through a per-repo cache root. After creation, those directories live as symlinks (or junctions on Windows) pointing into the cache, so the very first `npm install` (or `cargo build`, etc.) populates the cache once and every subsequent worktree gets it for free.

**In scope**
- New config keys: `worktreeLinkPatterns`, `worktreeLinkStrategy`, `worktreeLinkCacheDir`.
- New module `src/files/links.rs` implementing the link operation and its inverse (unlink on delete).
- Integration with `WorktreeService::create_worktree` (after copy, before post-create) and `WorktreeService::delete_worktree` (cleanup).
- Cache root layout, GC, and a `wisetree cache` subcommand for inspection/cleanup.
- Cross-platform symlink/junction handling (macOS, Linux, Windows).
- Settings screen view for inspection.
- Tests covering link creation, idempotency, deletion safety, Windows-specific paths (gated behind `cfg(windows)`).

**Out of scope (explicitly defer)**
- Linking *files* (only directories supported in v1; a `node_modules` of millions of small files is the entire point).
- Linking across repositories (cache is keyed on `git_root`, not global).
- Linking with hardlinks or reflinks. Symlinks/junctions only — they're the only thing that works portably for directory trees.

---

## 2. Configuration changes

Extend `WorktreeConfig` in `src/config/schema.rs`. Preserve `serde(deny_unknown_fields)` and the camelCase wire format:

```rust
/// Glob-or-literal directory patterns to symlink into the new worktree
/// from the per-repo cache root. Default empty (opt-in).
#[serde(rename = "worktreeLinkPatterns", default)]
pub worktree_link_patterns: Vec<String>,

/// Strategy when the source directory inside the worktree is missing.
/// Default: `LinkStrategy::CreateEmpty`.
#[serde(rename = "worktreeLinkStrategy", default)]
pub worktree_link_strategy: LinkStrategy,

/// Override the cache root. When `None`, defaults to
/// `~/.wisetree/cache/<repo-id>/` where `<repo-id>` is a stable hash
/// of the canonical `git_root` path. Use `$BASE_PATH`, `$WORKTREE_PATH`,
/// `$BRANCH_NAME`, `$SOURCE_BRANCH` for templating.
#[serde(rename = "worktreeLinkCacheDir", default)]
pub worktree_link_cache_dir: Option<String>,
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum LinkStrategy {
    /// Create an empty directory in the cache and symlink to it. Most
    /// useful for `node_modules/` etc., where post-create commands will
    /// populate the cache on first run.
    CreateEmpty,
    /// Copy the existing directory from the source worktree into the cache
    /// (cost incurred once), then symlink. Useful when bootstrapping from
    /// an existing main checkout that already has the deps installed.
    SeedFromSource,
    /// Skip if the source directory does not yet exist; otherwise behave
    /// like `SeedFromSource`. Safest for first-time setups.
    SeedIfPresent,
}

impl Default for LinkStrategy {
    fn default() -> Self { LinkStrategy::CreateEmpty }
}
```

Defaults stay empty so existing behavior is unchanged for users who don't opt in. Update the JSON schema and `tests/config.rs` round-trip cases.

The Settings screen (`src/tui/screens/settings.rs`) gains read-only views for `LinkPatterns`, `LinkStrategy`, and `LinkCacheDir`, paralleling how `CopyPatterns` is exposed today.

---

## 3. Cache layout

Cache root: `~/.wisetree/cache/<repo-id>/` (or whatever `worktreeLinkCacheDir` resolves to). `<repo-id>` is `blake3::hash(canonical(git_root).to_string())[..16]` — short, deterministic, no collisions in practice. Use the `blake3` crate; if you'd rather not add a dependency, `sha2` is already transitive via `reqwest` — verify with `cargo tree` and use whichever is already in the lock file.

Inside the repo's cache directory:

```
~/.wisetree/cache/<repo-id>/
  metadata.json        # { "gitRoot": "/abs/path", "createdAt": ..., "patterns": [...] }
  entries/
    node_modules/      # the canonical shared directory tree
    target/
    .venv/
```

Each pattern from `worktreeLinkPatterns` maps to one entry under `entries/`. The relative path inside the worktree mirrors the cache layout — `entries/foo/bar/` means worktrees get `<worktree>/foo/bar -> <cache>/entries/foo/bar`.

`metadata.json` is rewritten atomically (write-temp-then-rename) on every link operation. It is the source of truth for `wisetree cache list` and for orphan detection.

---

## 4. Linking module

Create `src/files/links.rs` with:

```rust
pub struct LinkReport {
    pub linked: Vec<LinkedEntry>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

pub struct LinkedEntry {
    pub pattern: String,
    pub cache_path: PathBuf,
    pub link_path: PathBuf,
    pub seeded: bool,
}

pub async fn link_patterns(
    source_dir: &Path,
    target_dir: &Path,
    cache_dir: &Path,
    config: &WorktreeConfig,
) -> LinkReport;

pub async fn unlink_patterns(
    target_dir: &Path,
    config: &WorktreeConfig,
) -> Result<()>;
```

**Algorithm for `link_patterns`** (per pattern):
1. Resolve the literal pattern path inside the worktree (e.g. `node_modules` → `<target_dir>/node_modules`). Globs are supported but expanded against the *source* worktree, not the cache.
2. Compute the cache entry: `<cache_dir>/entries/<relative_pattern>`.
3. If the cache entry already exists *and* is a directory, proceed to step 5.
4. Otherwise, materialize it according to `LinkStrategy`:
   - `CreateEmpty`: `tokio::fs::create_dir_all(cache_entry)`.
   - `SeedFromSource`: copy `<source_dir>/<pattern>` to `<cache_entry>` recursively. If the source is missing, fall through to `CreateEmpty` and record a `skipped` reason.
   - `SeedIfPresent`: same as `SeedFromSource` but record `skipped` and *do not* create the cache entry when the source is missing.
5. If `<target_dir>/<pattern>` already exists:
   - If it is a symlink to the same cache entry, skip — it's already linked.
   - If it is a real directory, refuse to delete it. Record an error and continue. (Better to fail loud than to silently nuke 500MB of `node_modules` the user just built.)
6. Otherwise, create the symlink. Use `tokio::fs::symlink` on Unix and `std::os::windows::fs::symlink_dir` on Windows; if creation fails on Windows because the user lacks `SeCreateSymbolicLinkPrivilege`, fall back to a directory junction via `mklink /J` invoked through `cmd.exe`. Record both successes and fallbacks in `LinkedEntry::seeded`.

**Algorithm for `unlink_patterns`**:
- Iterate the worktree's `worktreeLinkPatterns`. For each, if the path inside the worktree is a symlink (or Windows junction), remove it. Real directories are left alone — they are not the cache's property.
- Do **not** delete the cache entry. That's the entire point of sharing it. `wisetree cache prune` is the only operation that touches the cache itself.

---

## 5. Service integration

`src/worktree/service.rs::create_worktree`:
1. Existing: create the worktree via `git worktree add`.
2. Existing: copy patterns via `FileService::copy_files`.
3. **New**: if `config.worktree_link_patterns` is non-empty, call `link_patterns` and add the `LinkReport` to `CreateOutcome` (extend the struct). Surface the report in the TUI alongside the existing copy report.
4. Existing: run post-create commands.
5. Existing: launch terminal.

`src/worktree/service.rs::delete_worktree`:
- Before `git worktree remove`, call `unlink_patterns(worktree_path, config)`. This converts symlinks into "absent" so `git worktree remove` doesn't traverse into the cache.
- After successful removal, also rewrite `metadata.json` to drop the worktree from the `users` list (see §6).

`CreateOutcome` grows a `link_report: Option<LinkReport>` field. The TUI's create-summary screen (or whichever surface renders `CreateOutcome` today) gains a section for it. Mirror the existing copy-report rendering exactly.

---

## 6. Cache subcommand

Add a new top-level mode `Cache` with subcommands:

```
wisetree cache list             # show cache entries, total size, which worktrees use them
wisetree cache prune            # delete entries no worktree references
wisetree cache clear            # delete the entire cache for this repo (with confirmation)
wisetree cache path             # print absolute cache root, suitable for `cd $(wisetree cache path)`
```

Wire-up:
- `src/cli/args.rs`: add `AppMode::Cache` and a `CliCommand::Cache { action: CacheAction }`. `CacheAction` enum: `List`, `Prune`, `Clear`, `Path`.
- `src/cli/commands/cache.rs`: new module. `run(service, action) -> Result<()>`.
- `metadata.json` tracks a `users: Vec<{ worktreePath, lastSeen }>` list. `prune` is conservative: it only deletes entries with zero current users, and `lastSeen` older than 14 days. `clear` ignores both checks but prompts via `ConfirmDialog` in the TUI or refuses without `--force` on the CLI.

The TUI gains a `CacheScreen` reachable from the menu — but only when `worktreeLinkPatterns` is non-empty. The screen lists entries with size, age, and user count, with `d` to delete an entry and `Esc` to return.

---

## 7. Cross-platform notes

- **macOS / Linux**: `tokio::fs::symlink` (Unix) creates symlinks atomically. No special privileges required.
- **Windows**:
  - `std::os::windows::fs::symlink_dir` requires Developer Mode enabled OR the `SeCreateSymbolicLinkPrivilege`. Detect failure (`ERROR_PRIVILEGE_NOT_HELD`, raw OS error 1314) and fall back to `cmd /c mklink /J <link> <target>`. Junctions don't require admin and behave like symlinks for most workloads.
  - Path separators: use `Path::join`, never string concat.
  - When deleting, junctions need `RemoveDirectoryW` (which `std::fs::remove_dir` does) — not `DeleteFile`. Don't use `remove_file`.
- **All platforms**: never `canonicalize()` a path before checking if it's a symlink — that resolves the link. Use `symlink_metadata` exclusively for the "is this a symlink?" probe.

Add `cfg`-gated tests in `tests/file_links.rs` for the Windows junction path. Skip on non-Windows hosts with `#[cfg(windows)]`.

---

## 8. Tests

- `tests/file_links.rs` (new):
  - `link_patterns` with `CreateEmpty` creates the cache entry and the symlink.
  - Idempotent: running twice is a no-op (no errors, no duplicates).
  - Refusal: a pre-existing real directory at the target path produces an error, and the directory is left intact.
  - `SeedFromSource` copies the source directory's contents into the cache the first time and only the first time.
  - `unlink_patterns` removes only symlinks/junctions and leaves real directories alone.
  - Windows-only: junction fallback path produces a working junction.
- `tests/worktree_service.rs`: extend with a fixture that includes `worktreeLinkPatterns: ["node_modules"]`, asserts `CreateOutcome::link_report` is populated, and asserts the worktree path contains a symlink.
- `tests/cli_args.rs`: parse `wisetree cache list`, `wisetree cache prune`, etc.
- `tests/config.rs`: round-trip the new fields, including `LinkStrategy` variants.
- `tests/cli_e2e.rs`: smoke test for `wisetree cache list --json`.

Use `tempfile::tempdir` and a real `git init` for fixtures, matching `tests/git_write.rs`.

---

## 9. Documentation

Update the README:
- Add `worktreeLinkPatterns`, `worktreeLinkStrategy`, `worktreeLinkCacheDir` rows to the configuration table.
- Add a worked example: a Node project where `node_modules/` is shared, showing the per-worktree disk savings.
- Add a `wisetree cache` subsection in the CLI section.
- Note the Windows junction fallback in a callout box.

Update `schema.json` via the existing generator binary (`src/bin/generate_schema.rs`).

---

## 10. Acceptance criteria

1. With `worktreeLinkPatterns: ["node_modules"]` and `worktreeLinkStrategy: "CreateEmpty"`, creating a new worktree results in `<worktree>/node_modules` being a symlink (or junction) into the cache, and `npm install` populates the cache on first run; subsequent worktrees inherit the populated cache for free.
2. Deleting a worktree removes the symlink without touching the cache.
3. `wisetree cache list` shows entries, sizes, and active users.
4. `wisetree cache prune` removes only orphan entries older than the threshold.
5. On Windows without admin, junctions are used transparently and the test suite passes under `cfg(windows)`.
6. Config defaults stay empty, so existing users see zero behavior change after upgrading.
7. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all` are clean.

---

## 11. Things to watch out for

- **Never delete a real directory at the link path.** The user may have already built `node_modules` there. Refuse, surface an error in the report, and let them resolve it manually.
- **Treat the cache as append-only inside the create flow.** Only `cache prune`/`cache clear` ever delete cache entries. The create path adds; it never removes.
- **Don't follow symlinks in `match_files`.** The existing `FileService::copy_files` walks via `walkdir`. Make sure `link_patterns` does *not* compose with `copy_files` in a way that ends up walking through a freshly-created symlink and copying the cache's contents back into the worktree. The order is: create worktree → copy → link → post-create. Linking happens after copy precisely to avoid this.
- **Bun, pnpm, and pip have opinions.** `pnpm` already symlinks inside `node_modules`; pointing `node_modules` itself at a shared dir mostly works but breaks `pnpm`'s store integrity check unless `node-linker=hoisted`. Document this in the README and let the user choose. We do not silently disable for them.
- **Test on a real network filesystem if possible.** Symlinks across NFS/SMB are flaky; document "local filesystems only" if you can't make it robust.
- **`metadata.json` writes must be atomic.** Use `tempfile::NamedTempFile::persist` or write-then-rename. A half-written metadata file breaks every subsequent operation.
- **Keep `LinkReport` separate from `CopyReport`.** They look similar; they are not the same. Conflating them in the TUI surfaces is a refactor trap — accept the duplication.
