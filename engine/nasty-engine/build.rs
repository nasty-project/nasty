fn main() {
    let now = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NASTY_BUILD_DATE={}", now.trim());

    // Bake the git commit this engine was built from into the binary as
    // `NASTY_GIT_SHA`, readable at runtime via `option_env!`. The engine
    // already relied on this for telemetry, but only `nasty-system`'s
    // build.rs emitted it — so a plain `cargo build` of `nasty-engine`
    // left `option_env!("NASTY_GIT_SHA")` as `None` in *this* crate, and
    // `/health` / `--version` couldn't report the commit. Emit it here too
    // so cargo dev builds carry the SHA, not just Nix builds.
    //
    // Sources, in priority order (mirrors nasty-system/build.rs):
    //   1. `NASTY_GIT_SHA` env var — Nix passes the flake rev (mkEngine),
    //      the only source that works in the sandbox (no .git there).
    //   2. `git rev-parse HEAD` — local cargo builds outside Nix.
    // On total failure the SHA is just absent; builds always succeed.
    let sha = std::env::var("NASTY_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    if let Some(sha) = sha {
        println!("cargo:rustc-env=NASTY_GIT_SHA={sha}");
    }
    println!("cargo:rerun-if-env-changed=NASTY_GIT_SHA");
}
