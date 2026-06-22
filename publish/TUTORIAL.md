# 📦 Publishing Wisetree

This tutorial is the operational checklist for cutting a new `wisetree`
release and pushing it to both **npm** and **Homebrew**. Follow the
sections **top to bottom** — each step assumes the previous ones
completed cleanly.

The release pipeline is driven by [`cargo-dist`](https://github.com/axodotdev/cargo-dist),
configured under `[workspace.metadata.dist]` in `Cargo.toml`. The
GitHub Actions workflow at `.github/workflows/release.yml` fans out on
every signed `v*` tag, builds binaries for the five supported targets,
uploads them to GitHub Releases, publishes the npm umbrella + platform
packages, and pushes an updated formula to `victorcorcos/homebrew-tap`.

> **Current version:** `1.0.0` — replace every `1.x.y` placeholder
> below with the version you are about to ship (`1.0.0`, `1.0.1`,
> `1.1.0`, etc.).

---

## 1. Prerequisites

A one-time setup per machine.

### Tooling

```bash
# Rust toolchain pinned by rust-toolchain.toml
rustc --version
cargo --version

# Node.js & npm (for the npm publish path, even when CI does the work)
node --version    # >= 14
npm --version

# Homebrew (only required if you ever audit the formula locally)
brew --version

# GitHub CLI — used to sanity-check Actions runs and download artifacts
gh --version

# jq — used by the npm-version bump one-liner in Section 3
jq --version
```

### GitHub repository secrets

Confirm these are set under
**Settings → Secrets and variables → Actions** on
`github.com/victorcorcos/wisetree`:

| Secret              | Purpose                                                                                              |
| ------------------- | ---------------------------------------------------------------------------------------------------- |
| `NPM_TOKEN`         | npm **automation** token with publish rights on `wisetree` and every `wisetree-<platform>` package. |
| `HOMEBREW_TAP_TOKEN`| GitHub Personal Access Token with `contents: write` on `victorcorcos/homebrew-tap`.                 |

Generate / rotate the npm token at
<https://www.npmjs.com/settings/<your-user>/tokens>, and the GitHub PAT
at <https://github.com/settings/tokens> (classic, `repo` scope is fine).

### Local credentials (only needed for a manual publish fallback)

```bash
npm login                       # interactive — writes ~/.npmrc
npm whoami                      # confirm the active user
```

For Homebrew the tap is a plain git repository — `git push` permissions
on `victorcorcos/homebrew-tap` are all you need.

---

## 2. Pre-release checklist

Run from a clean checkout of `main`:

```bash
git checkout main
git pull --ff-only
git status                      # must be clean
```

### Run the full test + lint matrix

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

If any of these fail, **stop**. Fix on `main` (or via a PR) before
tagging.

### Smoke-test the binary

```bash
cargo build --release
./target/release/wisetree --version
./target/release/wisetree --help
```

### Confirm `cargo-dist` is happy

```bash
cargo install cargo-dist@0.23.0   # pin to the version used by the workflow
cargo dist plan                   # prints the artifacts the next tag will produce
```

The pinned version must match `cargo-dist-version` in `Cargo.toml` and the
installer URL in `.github/workflows/release.yml` (both currently `0.23.0`).

> ⚠️ **The release workflow is hand-edited.** cargo-dist 0.23.0's
> template emits `runs-on: ubuntu-20.04`, which GitHub Actions retired
> in 2025 — release jobs queue forever on that label. We pinned
> `ubuntu-22.04` in `.github/workflows/release.yml` and set
> `allow-dirty = ["ci"]` in `[workspace.metadata.dist]` so `cargo dist
> plan` accepts the manual edit. If you ever re-run `cargo dist init`
> or `cargo dist generate`, **re-apply the ubuntu pin** before pushing:
> `sed -i'' -e 's/ubuntu-20\.04/ubuntu-22.04/g' .github/workflows/release.yml`.

The output should list five targets:
`aarch64-apple-darwin`, `x86_64-apple-darwin`,
`aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`,
`x86_64-pc-windows-msvc`.

---

## 3. Bumping the version

The version must be **identical** across eight locations: `Cargo.toml`,
the six npm `package.json` files (the umbrella `wisetree` package plus
its five platform packages), and the Homebrew formula baseline. Pick
the new version (e.g. `1.0.1`) and update each one.

### 3.1 — Rust crate

`Cargo.toml`:

```toml
[package]
name = "wisetree"
version = "1.0.1"
```

Re-run `cargo build` so `Cargo.lock` updates:

```bash
cargo build --release
```

### 3.2 — npm umbrella package

`npm/wisetree/package.json` — update **both** `version` and every
`optionalDependencies` entry so they pin the exact platform-package
version:

```json
{
  "name": "wisetree",
  "version": "1.0.1",
  "optionalDependencies": {
    "wisetree-darwin-arm64": "1.0.1",
    "wisetree-darwin-x64":   "1.0.1",
    "wisetree-linux-x64-gnu":   "1.0.1",
    "wisetree-linux-arm64-gnu": "1.0.1",
    "wisetree-win32-x64-msvc":  "1.0.1"
  }
}
```

### 3.3 — npm platform packages

Update `version` in each of the five platform packages:

```bash
npm/wisetree-darwin-arm64/package.json
npm/wisetree-darwin-x64/package.json
npm/wisetree-linux-arm64-gnu/package.json
npm/wisetree-linux-x64-gnu/package.json
npm/wisetree-win32-x64-msvc/package.json
```

A one-liner that flips every npm version at once (run from the repo
root):

```bash
NEW=1.0.1
for f in npm/*/package.json; do
  tmp=$(mktemp)
  jq --arg v "$NEW" '
    .version = $v
    | if .optionalDependencies then
        .optionalDependencies |= with_entries(.value = $v)
      else . end
  ' "$f" > "$tmp" && mv "$tmp" "$f"
done
```

### 3.4 — Homebrew formula baseline

`homebrew-tap/Formula/wisetree.rb` is regenerated by `cargo-dist` on
every release, but the in-repo copy is still the reviewable baseline.
Update the version line and reset the SHAs to the sentinel so reviewers
can tell unreleased changes from a stale checked-in formula:

```rb
class Wisetree < Formula
  version "1.0.1"
  # ...
  sha256 "REPLACE_WITH_RELEASE_SHA"
end
```

CI will replace `REPLACE_WITH_RELEASE_SHA` with the real digests after
the tag publishes.

### 3.5 — Verify all version references agree

```bash
grep -nE '"version"[[:space:]]*:|^version = "|^[[:space:]]*version "' \
  Cargo.toml \
  npm/wisetree/package.json \
  npm/wisetree-darwin-arm64/package.json \
  npm/wisetree-darwin-x64/package.json \
  npm/wisetree-linux-arm64-gnu/package.json \
  npm/wisetree-linux-x64-gnu/package.json \
  npm/wisetree-win32-x64-msvc/package.json \
  homebrew-tap/Formula/wisetree.rb
```

Every match should reference the same `1.0.1` (the npm umbrella package
will show it once for `version` and once per `optionalDependencies`
entry — that is expected).

### 3.6 — Commit the bump

```bash
git checkout -b release/v1.0.1
git add -A
git commit -m "Release v1.0.1"
git push -u origin release/v1.0.1
gh pr create \
  --title "Release v1.0.1" \
  --body  "Version bump for v1.0.1"
```

Wait for CI to go green, then merge the PR into `main`.

---

## 4. Tag, push, and let CI publish

After the bump PR lands on `main`:

```bash
git checkout main
git pull --ff-only
git tag -a v1.0.1 -m "v1.0.1"
git push origin v1.0.1
```

The push of a `v*` tag triggers `.github/workflows/release.yml`, which:

1. Builds release binaries for the five `cargo-dist` targets.
2. Creates GitHub Release `v1.0.1` and attaches the `.tar.xz` /
   `.zip` artifacts + their SHAs.
3. Publishes the umbrella `wisetree` package and the five
   `wisetree-<platform>` packages to npm.
4. Pushes an updated `wisetree.rb` (with real SHAs) to
   `victorcorcos/homebrew-tap`.

Watch the run:

```bash
gh run watch                       # picks the most recent workflow run
gh release view v1.0.1 --web       # open the release page in a browser
```

---

## 5. Verifying the npm publish

Once the workflow succeeds:

```bash
npm view wisetree version          # should print 1.0.1
npm view wisetree dist-tags

# Spot-check every platform package
for p in wisetree-darwin-arm64 wisetree-darwin-x64 \
         wisetree-linux-arm64-gnu wisetree-linux-x64-gnu \
         wisetree-win32-x64-msvc; do
  echo "$p: $(npm view "$p" version)"
done
```

End-to-end install on a clean directory:

```bash
mkdir -p /tmp/wisetree-npm-smoketest && cd /tmp/wisetree-npm-smoketest
npm init -y > /dev/null
npm install -g wisetree@1.0.1
wisetree --version                 # → Wisetree v1.0.1
```

If a package failed to publish (rare, usually a transient 5xx from the
registry), you can re-run the publish job from the Actions UI **or** run
a manual publish (see Section 7 — _Manual fallback_).

---

## 6. Verifying the Homebrew publish

The tap is the canonical install channel until `homebrew/core`
notability is reached (see README).

```bash
brew update
brew info victorcorcos/tap/wisetree         # should report 1.0.1
brew install victorcorcos/tap/wisetree
wisetree --version                          # → Wisetree v1.0.1
```

If you already have `wisetree` installed:

```bash
brew upgrade victorcorcos/tap/wisetree
```

Inspect the formula that CI just pushed:

```bash
gh repo view victorcorcos/homebrew-tap --web
# or, locally:
git clone https://github.com/victorcorcos/homebrew-tap /tmp/wisetree-tap
grep -E 'version|sha256' /tmp/wisetree-tap/Formula/wisetree.rb
```

Every `sha256` must be a real 64-char hex digest — **no remaining
`REPLACE_WITH_RELEASE_SHA` lines**. If any are still placeholders, the
homebrew publish job failed and needs to be re-run.

---

## 7. Manual fallback (only if CI is broken)

Run **only** if the workflow cannot be repaired in time and you need to
ship the release by hand.

### 7.1 — Manual npm publish

```bash
# Make sure you are logged in as a user with publish rights
npm whoami

# Publish the five platform packages first (umbrella depends on them)
for p in wisetree-darwin-arm64 wisetree-darwin-x64 \
         wisetree-linux-arm64-gnu wisetree-linux-x64-gnu \
         wisetree-win32-x64-msvc; do
  ( cd "npm/$p" && npm publish --access public )
done

# Then publish the umbrella package
( cd npm/wisetree && npm publish --access public )

# Sanity-check
npm view wisetree version
```

### 7.2 — Manual Homebrew tap update

```bash
# Download the release artifacts directly from GitHub Releases so the SHAs
# match exactly what users will fetch.
mkdir -p /tmp/wisetree-artifacts
gh release download v1.0.1 \
  --repo victorcorcos/wisetree \
  --pattern '*.tar.xz' \
  --dir /tmp/wisetree-artifacts

# Compute SHA-256 digests. Use shasum on macOS, sha256sum on Linux:
( cd /tmp/wisetree-artifacts && \
  ( command -v sha256sum >/dev/null && sha256sum *.tar.xz \
                                    || shasum -a 256 *.tar.xz ) )

# Now update the tap.
git clone https://github.com/victorcorcos/homebrew-tap /tmp/wisetree-tap
cd /tmp/wisetree-tap

# Edit Formula/wisetree.rb to set version and each sha256 value, then:
git add Formula/wisetree.rb
git commit -m "wisetree 1.0.1"
git push origin main
```

### 7.3 — Verify the manual publish

Repeat **Section 5** and **Section 6** in full.

---

## 8. Post-release housekeeping

1. **Announce** the release in the project README changelog (if you
   keep one) and on any relevant channels.
2. **Yank a bad release** if you discover a regression:
   ```bash
   npm deprecate wisetree@1.0.1 "Use 1.0.2 — fixes #N"
   ```
   For Homebrew, push a revert commit on the tap. **Never** delete a
   published GitHub Release tag — bump and ship `1.0.2` instead.
3. **Sanity-check the in-app "Check for Updates" screen** by launching
   `wisetree`, opening **Settings → 9. Check for Updates**, and
   confirming both the `npm` and `homebrew` rectangles report the new
   `1.0.1` version. That screen re-fetches from both sources on entry,
   so no cache reset is required.

---

## 9. Quick reference — full release in one block

```bash
# 0. Sanity
git checkout main && git pull --ff-only
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all

# 1. Bump (replace 1.0.1)
NEW=1.0.1
sed -i.bak -E "s/^version = \".+\"/version = \"$NEW\"/" Cargo.toml && rm Cargo.toml.bak
for f in npm/*/package.json; do
  tmp=$(mktemp)
  jq --arg v "$NEW" '
    .version = $v
    | if .optionalDependencies then
        .optionalDependencies |= with_entries(.value = $v)
      else . end
  ' "$f" > "$tmp" && mv "$tmp" "$f"
done
sed -i.bak -E "s/version \".+\"/version \"$NEW\"/" \
  homebrew-tap/Formula/wisetree.rb && rm homebrew-tap/Formula/wisetree.rb.bak
cargo build --release

# 2. Commit + PR + merge
git checkout -b "release/v$NEW"
git add -A
git commit -m "Release v$NEW"
git push -u origin "release/v$NEW"
gh pr create --title "Release v$NEW" --body "Version bump for v$NEW"
# (merge the PR via GitHub UI)

# 3. Tag and let CI publish
git checkout main && git pull --ff-only
git tag -a "v$NEW" -m "v$NEW"
git push origin "v$NEW"
gh run watch

# 4. Verify
npm view wisetree version
brew update && brew info victorcorcos/tap/wisetree
```
