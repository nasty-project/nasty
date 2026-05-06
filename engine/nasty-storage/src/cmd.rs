use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

/// Execute a command and return its output.
/// All system commands go through here for logging and future testability.
pub async fn run(program: &str, args: &[&str]) -> std::io::Result<Output> {
    debug!("exec: {} {}", program, args.join(" "));
    Command::new(program).args(args).output().await
}

/// Execute a command with data piped to stdin.
pub async fn run_stdin(program: &str, args: &[&str], stdin_data: &[u8]) -> std::io::Result<Output> {
    debug!("exec (stdin): {} {}", program, args.join(" "));
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_data).await?;
        // Drop to close stdin so the process can proceed
    }

    child.wait_with_output().await
}

/// Execute a command with stdin, returning stdout on success or stderr on failure.
pub async fn run_ok_stdin(
    program: &str,
    args: &[&str],
    stdin_data: &[u8],
) -> Result<String, String> {
    let output = run_stdin(program, args, stdin_data)
        .await
        .map_err(|e| format!("failed to execute {program}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{program} exited with {}: {stderr}", output.status))
    }
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
        Err(format!("{program} exited with {}: {stderr}", output.status))
    }
}
