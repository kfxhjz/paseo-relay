//! Embed a build revision for `/version` and the `paseo_relay_build_info` metric.
//!
//! Local builds derive it from git. The release workflow passes
//! `PASEO_RELAY_BUILD_REVISION` explicitly because the Docker build context does
//! not include `.git`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PASEO_RELAY_BUILD_REVISION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");

    let revision = std::env::var("PASEO_RELAY_BUILD_REVISION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_owned());

    println!(
        "cargo:rustc-env=PASEO_RELAY_BUILD_REVISION={}",
        sanitize(&revision)
    );
}

/// Keep the embedded revision safe for JSON, Prometheus labels, and logs.
fn sanitize(value: &str) -> String {
    let mut sanitized: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(64)
        .collect();
    if sanitized.is_empty() {
        sanitized.push_str("unknown");
    }
    sanitized
}

fn git_revision() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!revision.is_empty()).then_some(revision)
}
