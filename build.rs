// build.rs — stamp the commit this binary was built from, so `daemon-status` can
// name the build. `scripts/fetch-or-build.sh` refuses a prebuilt whose release
// commit is not the checkout's HEAD; the stamp makes the same fact readable
// after the fact, next to what `herdr plugin list` reports for the clone.
// The release workflow passes SCUTTLEBUTT_COMMIT; every other build reads git.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SCUTTLEBUTT_COMMIT");
    let commit = std::env::var("SCUTTLEBUTT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(head_commit)
        .unwrap_or_else(|| "unknown".into());
    let short: String = commit.trim().chars().take(7).collect();
    println!("cargo:rustc-env=SCUTTLEBUTT_COMMIT={short}");
}

/// Also registers the files a commit change touches, since naming any
/// rerun-if-changed replaces cargo's default of rerunning on every source edit.
fn head_commit() -> Option<String> {
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    git(&["rev-parse", "HEAD"])
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
