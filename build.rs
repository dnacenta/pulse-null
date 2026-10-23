//! Build-time facts the binary reports about itself.
//!
//! `PULSE_NULL_GIT_COMMIT`: the commit the binary was built from, recorded
//! on every probe row (PN-144, G6/G7 lineage). `unknown` when the source is
//! not a git checkout (a crates.io build). A pre-set value wins, so a
//! packager can pass it in.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PULSE_NULL_GIT_COMMIT");
    if let Ok(commit) = std::env::var("PULSE_NULL_GIT_COMMIT") {
        println!("cargo:rustc-env=PULSE_NULL_GIT_COMMIT={commit}");
        return;
    }
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Rebuild when HEAD moves: HEAD itself and the ref it points at.
    for path in [
        git(&["rev-parse", "--git-path", "HEAD"]),
        git(&["symbolic-ref", "-q", "HEAD"]).and_then(|r| git(&["rev-parse", "--git-path", &r])),
    ]
    .into_iter()
    .flatten()
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rustc-env=PULSE_NULL_GIT_COMMIT={commit}");
}
