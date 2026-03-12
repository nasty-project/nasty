use std::process::Output;
use tokio::process::Command;
use tracing::debug;

/// Execute a command and return its output.
/// All system commands go through here for logging and future testability.
pub async fn run(program: &str, args: &[&str]) -> std::io::Result<Output> {
    debug!("exec: {} {}", program, args.join(" "));
    Command::new(program).args(args).output().await
}

/// Execute a command, returning stdout as String on success or an error message on failure.
pub async fn run_ok(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run(program, args)
        .await
        .map_err(|e| format!("failed to execute {program}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{program} exited with {}: {stderr}",
            output.status
        ))
    }
}
