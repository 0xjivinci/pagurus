//! Stamps the build with its git commit and date, so `pagurus --version` says
//! exactly which code runs. Docker and release builds may have no git inside:
//! they pass PAGURUS_COMMIT / PAGURUS_DATE instead. Without either, both are empty.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-env-changed=PAGURUS_COMMIT");
    println!("cargo:rerun-if-env-changed=PAGURUS_DATE");
    println!("cargo:rerun-if-changed=src"); // for the "+" (uncommitted changes) mark
    // a new commit or checkout changes HEAD or the branch it points at
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            let common = git(&["rev-parse", "--git-common-dir"]).unwrap_or(dir);
            println!("cargo:rerun-if-changed={common}/{r}");
            println!("cargo:rerun-if-changed={common}/packed-refs");
        }
    }
    let commit = std::env::var("PAGURUS_COMMIT").ok().filter(|s| !s.is_empty()).or_else(|| {
        let c = git(&["rev-parse", "--short=7", "HEAD"])?;
        // uncommitted changes in tracked files: say so
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
        Some(if dirty { format!("{c}+") } else { c })
    });
    let date = std::env::var("PAGURUS_DATE").ok().filter(|s| !s.is_empty()).or_else(|| git(&["log", "-1", "--format=%cs"]));
    println!("cargo:rustc-env=PAGURUS_COMMIT={}", commit.unwrap_or_default());
    println!("cargo:rustc-env=PAGURUS_DATE={}", date.unwrap_or_default());
}
