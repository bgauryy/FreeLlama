use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `<commit>` or `<commit>-dirty` (tracked files modified); `unknown` without git (crates.io).
/// Embedded as `freellama-source-commit=<value>` so `scripts/verify-release-packages.mjs` can
/// reject an artifact built from other source without running it.
fn source_fingerprint() -> String {
    let Some(commit) = git(&["rev-parse", "HEAD"]).filter(|sha| !sha.is_empty()) else {
        return "unknown".to_owned();
    };
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_none_or(|s| !s.is_empty());
    if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    }
}

fn main() {
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = Path::new(&git_dir);
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(reference).display()
            );
        }
    }
    // An unstaged edit changes neither HEAD nor the index, so watch the sources that end up in
    // the artifacts too; otherwise a build after such an edit keeps an earlier clean fingerprint.
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        let root = Path::new(&root);
        for watched in [
            "packages/rust-core/src",
            "packages/cli/src",
            "Cargo.toml",
            "Cargo.lock",
        ] {
            println!("cargo:rerun-if-changed={}", root.join(watched).display());
        }
    }
    println!(
        "cargo:rustc-env=FREELLAMA_SOURCE_COMMIT={}",
        source_fingerprint()
    );
    // Only wires up napi-rs's build glue when the `napi` feature is active (Node-addon build).
    // A normal `cargo build`/`cargo test` never touches this — see Cargo.toml's `[features]`.
    // `napi-build` is an optional dependency, so the call itself must be compile-time gated, not
    // just skipped at runtime, or a feature-less build would fail to resolve the crate at all.
    #[cfg(feature = "napi")]
    napi_build::setup();
}
