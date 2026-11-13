//! Puts the git commit into the program, so that the version shown in the page says which build it is.
//!
//! The commit comes from the environment variable `NICOCHESS_COMMIT` when it is set (the Docker
//! builds cannot run git: the build scripts give it), otherwise from git itself. A build from a
//! tree with uncommitted changes says so ("-dirty"). Without either, the commit is left out.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=NICOCHESS_COMMIT");
    // The commit changes with HEAD; "dirty" changes with the files.
    for path in [".git/HEAD", ".git/index", "src", "web", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let from_environment = std::env::var("NICOCHESS_COMMIT").ok().filter(|value| !value.trim().is_empty());
    let commit = from_environment.or_else(commit_from_git).unwrap_or_default();
    println!("cargo:rustc-env=NICOCHESS_COMMIT={commit}");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok().filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn commit_from_git() -> Option<String> {
    let hash = git(&["rev-parse", "--short=8", "HEAD"])?;
    let dirty = git(&["status", "--porcelain"]).is_some_and(|changes| !changes.is_empty());
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}
