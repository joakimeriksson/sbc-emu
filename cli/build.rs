//! The git commit the binary was built from, for `sbc-emu --version`. Absent when the sources are
//! not a git checkout of this repository (a release tarball) or git is not installed.
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !text.trim().is_empty()).then(|| text.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // the workspace root (cli/..) must be the checkout's top: sources unpacked inside some other
    // repository are not a checkout of this one
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("..");
    let top = git(&["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    if top.and_then(|t| t.canonicalize().ok()) != root.canonicalize().ok() {
        return;
    }
    if let Some(commit) = git(&["rev-parse", "--short=10", "HEAD"]) {
        println!("cargo:rustc-env=SBC_EMU_COMMIT={commit}");
    }
    // rebuild when HEAD moves. logs/HEAD (the reflog) grows on every commit, checkout and reset,
    // also when the branch lives only in packed-refs, where a commit then writes a loose ref file
    // that was not there to watch. Only files that exist: cargo reruns the script on every build
    // for a missing one.
    let mut watch = vec!["logs/HEAD".to_string(), "HEAD".to_string(), "packed-refs".to_string()];
    watch.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for f in watch {
        if let Some(path) = git(&["rev-parse", "--git-path", &f]) {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
