use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

/// Execute a command and return its output.
/// All system commands go through here for logging and future testability.
pub async fn run(program: &str, args: &[&str]) -> std::io::Result<Output> {
    debug!("exec: {} {}", program, args.join(" "));
    timed(program, Command::new(program).args(args).output()).await
}

/// Record the command as a diagnostics stage, like `nasty_common::cmd` does.
async fn timed(
    program: &str,
    output: impl std::future::Future<Output = std::io::Result<Output>>,
) -> std::io::Result<Output> {
    let mut timing =
        nasty_common::diagnostics::Stage::new(nasty_common::diagnostics::command_stage(program));
    let result = output.await;
    timing.finish(result.as_ref().is_ok_and(|o| o.status.success()));
    result
}

/// Execute a CPU/IO-heavy command below control-plane priority.
pub async fn run_bulk(program: &str, args: &[&str]) -> std::io::Result<Output> {
    debug!("exec (bulk): {} {}", program, args.join(" "));
    timed(
        program,
        nasty_common::priority::bulk_command(program)
            .args(args)
            .output(),
    )
    .await
}

/// Execute a command with data piped to stdin.
pub async fn run_stdin(program: &str, args: &[&str], stdin_data: &[u8]) -> std::io::Result<Output> {
    debug!("exec (stdin): {} {}", program, args.join(" "));
    timed(program, async {
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
    })
    .await
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

/// Execute a bulk command, returning stdout on success or stderr on failure.
pub async fn run_ok_bulk(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run_bulk(program, args)
        .await
        .map_err(|e| format!("failed to execute {program}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{program} exited with {}: {stderr}", output.status))
    }
}

#[cfg(test)]
mod tests {
    use nasty_common::diagnostics;

    #[tokio::test]
    async fn commands_are_timed_during_detailed_capture() {
        diagnostics::detailed(true);
        let sequence = diagnostics::next_sequence();
        diagnostics::REQUEST
            .scope((sequence, "fs.list"), async {
                super::run("true", &[]).await.unwrap();
            })
            .await;
        diagnostics::detailed(false);
        assert!(
            diagnostics::snapshot()
                .0
                .iter()
                .any(|t| t.sequence == sequence && t.stage == "command.other" && t.outcome == "ok")
        );
    }
}
