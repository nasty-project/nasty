// Bakes the git commit this engine was built from into the binary
// as `NASTY_GIT_SHA`, readable at runtime via `option_env!`. The
// engine uses this as the authoritative answer to "what nasty rev
// am I?" — the binary literally was built from this commit, so
// there's no proxy chain to drift or lag.
//
// Priority of sources:
//   1. `NASTY_GIT_SHA` env var — Nix passes the flake's rev here
//      (set in `mkEngine` in flake.nix). This is the only source
//      that works in the Nix sandbox, which doesn't have access to
//      the .git directory.
//   2. `git rev-parse HEAD` — works for local cargo builds outside
//      Nix (developer dev loop). Silently no-ops if there's no git
//      checkout or git isn't on PATH.
//
// On total failure the SHA is just absent — `option_env!` returns
// None at runtime and the engine falls back to its existing
// /etc/nixos/flake.lock chain. Builds always succeed.

fn main() {
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
