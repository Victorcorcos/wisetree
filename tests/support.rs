use std::path::Path;
use std::process::Command;

pub fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("git invocation");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

pub fn init_repo_with_main(cwd: &Path) {
    let init_with_branch = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(cwd)
        .status()
        .expect("git init invocation");

    if init_with_branch.success() {
        return;
    }

    git(cwd, &["init", "-q"]);
    git(cwd, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}
