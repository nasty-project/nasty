//! Subprocess helpers with mandatory error logging.
//!
//! # Why this exists
//!
//! Every "this command quietly didn't work and we have no idea why" support
//! cycle in this project's history (see discussion #159, PRs #197–#200) traces
//! back to a `let _ = Command::new(...)...await` somewhere — exit codes and
//! stderr discarded, the failure invisible in logs, and the only way to
//! diagnose anything was to ship a dedicated visibility patch and ask the
//! reporter to retry.
//!
//! Routing every subprocess invocation through this module makes that class of
//! bug impossible: a non-zero exit *always* lands in the journal at `warn!`
//! with the program name, args, exit status, and stderr. A spawn failure
//! (binary missing, permission denied, etc.) does the same. Callers that need
//! the raw `Output` still get it back; callers that don't can use [`try_run`]
//! and rely on the logging alone.
//!
//! # Project rule (engine code)
//!
//! Use these helpers — `nasty_common::cmd::{run, run_ok, try_run}` — for every
//! subprocess invocation. Do not call `tokio::process::Command::new(...).output()`
//! or `.status()` directly unless you have a *specific* reason (e.g., long-lived
//! child where you stream stdout) and you've thought through the error paths.
//! When in doubt, use `run`.
//!
//! ```no_run
//! use nasty_common::cmd;
//! # async fn doc() -> Result<(), Box<dyn std::error::Error>> {
//! // Best-effort cleanup — discards Result, still logs spawn errors and
//! // non-zero exits.
//! cmd::try_run("systemctl", &["reset-failed", "some-unit"]).await;
//!
//! // Caller wants to react to success/failure — return Output.
//! let out = cmd::run("ip", &["link", "show"]).await?;
//! if !out.status.success() { /* ... */ }
//!
//! // Caller wants stdout-or-error-string semantics.
//! let stdout = cmd::run_ok("hostname", &[]).await?;
//! # Ok(()) }
//! ```

use std::process::Output;
use tokio::process::Command;
use tracing::{debug, warn};

/// Spawn a command, await it to completion, and return its `Output`.
///
/// Always logs:
/// - `debug!`  on entry (the program + args)
/// - `warn!`   if the spawn itself fails (e.g. binary not in PATH)
/// - `warn!`   if the command exits non-zero (with stderr)
///
/// Callers receive the same `Output` they would from `Command::output()`.
/// The logging happens before the value is returned, so a caller that
/// inspects `output.status` is *also* getting the failure into the journal
/// for free.
pub async fn run(program: &str, args: &[&str]) -> std::io::Result<Output> {
    debug!(target: "nasty::cmd", "exec: {} {}", program, args.join(" "));
    let result = Command::new(program).args(args).output().await;
    log_result(program, args, &result);
    result
}

/// Like [`run`] but returns `Ok(stdout)` on success or `Err(stderr-with-context)`
/// on either spawn failure or non-zero exit. Useful when the caller wants to
/// surface the error message back to a user (e.g., over JSON-RPC) rather than
/// react to it programmatically.
pub async fn run_ok(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run(program, args)
        .await
        .map_err(|e| format!("failed to spawn {program}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Best-effort: spawn the command, log any failure, discard the result.
///
/// Use for "try to clean up state but it's fine if it doesn't work" calls —
/// e.g. `systemctl reset-failed` before a `start`, or `chmod` after a write.
/// The non-zero exit will still appear in the journal at `warn!`, so a real
/// problem (binary missing, permission denied, the cleanup mattering after
/// all) is debuggable.
pub async fn try_run(program: &str, args: &[&str]) {
    let _ = run(program, args).await;
}

fn log_result(program: &str, args: &[&str], result: &std::io::Result<Output>) {
    match result {
        Err(e) => warn!(
            target: "nasty::cmd",
            "spawn failed: {} {} — {e}",
            program,
            args.join(" ")
        ),
        Ok(o) if !o.status.success() => warn!(
            target: "nasty::cmd",
            "non-zero exit ({}): {} {} — stderr: {}",
            o.status,
            program,
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_returns_output_on_success() {
        let out = run("true", &[]).await.expect("spawn true");
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn run_ok_returns_stdout_on_success() {
        let stdout = run_ok("printf", &["hello"]).await.expect("printf hello");
        assert_eq!(stdout, "hello");
    }

    #[tokio::test]
    async fn run_ok_returns_stderr_on_nonzero_exit() {
        let err = run_ok("false", &[]).await.expect_err("false should fail");
        assert!(err.contains("false exited with"), "got: {err}");
    }

    #[tokio::test]
    async fn run_ok_returns_spawn_error_when_binary_missing() {
        let err = run_ok("nasty-nonexistent-binary-xyz", &[])
            .await
            .expect_err("missing binary");
        assert!(err.contains("failed to spawn"), "got: {err}");
    }

    #[tokio::test]
    async fn try_run_does_not_panic_on_missing_binary() {
        // The point: `try_run` swallows the Result but still logs internally;
        // the caller never has to think about error handling.
        try_run("nasty-nonexistent-binary-xyz", &[]).await;
    }
}
