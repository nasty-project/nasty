use std::process::Command;

fn main() {
    // Prefer NASTY_GIT_COMMIT env var (set by Nix flake) over git command
    // (git isn't available in the Nix build sandbox)
    if std::env::var("NASTY_GIT_COMMIT").is_err() {
        let commit = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_else(|| "unknown".to_string());
        println!("cargo:rustc-env=NASTY_GIT_COMMIT={}", commit.trim());
    }

    let now = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NASTY_BUILD_DATE={}", now.trim());
}
