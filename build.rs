use std::path::Path;
use std::process::Command;

// One-shot installer for the repo's git hooks. Sets `core.hooksPath = githooks`
// the first time someone builds the repo after cloning, so the tracked hooks get
// picked up automatically. Never fails the build — if anything is off (no git,
// no .git dir, hooksPath already set), we just exit.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=githooks/pre-push");
    println!("cargo:rerun-if-changed=githooks/post-commit");

    let manifest_dir = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(dir) => dir,
        None => return,
    };

    if !Path::new(&manifest_dir).join(".git").exists() {
        return;
    }

    if !Path::new(&manifest_dir)
        .join("githooks")
        .join("pre-push")
        .exists()
    {
        return;
    }

    let already_set = Command::new("git")
        .args(["config", "--local", "--get", "core.hooksPath"])
        .current_dir(&manifest_dir)
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false);

    if already_set {
        return;
    }

    let _ = Command::new("git")
        .args(["config", "--local", "core.hooksPath", "githooks"])
        .current_dir(&manifest_dir)
        .status();
}
