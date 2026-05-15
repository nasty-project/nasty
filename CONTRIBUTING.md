# Contributing to NASty

## Logging rule

**Every operation must log enough that a failure can be diagnosed from the journal alone — without shipping a "ship visibility patch then ask the user to retry" round-trip.**

This rule exists because we've burned multi-PR cycles on bugs that were *visible all along* but our code threw the diagnostic away. Discussion #159 took four PRs (#197 → #198 → #199 → #200) to land because the engine was swallowing per-connection NetworkManager errors, then per-field DBus type-mismatch errors. Each "ship more logs and ask the reporter to re-run" trip cost a day of latency.

### How to comply

#### Subprocess invocations — use `nasty_common::cmd`

Always route subprocess calls through one of:

- `cmd::run(program, &[args])`           — returns `Output`, callers can react to status. Logs spawn failure + non-zero exit at `warn!`.
- `cmd::run_ok(program, &[args])`        — returns `Ok(stdout)` or `Err(stderr-with-context)`. Same logging.
- `cmd::try_run(program, &[args])`       — best-effort; discards the result but **still logs** spawn failure + non-zero exit at `warn!`. Use for "try to clean up, OK if it doesn't work".

Do **not** call `tokio::process::Command::new(...).output()` / `.status()` directly unless you have a specific reason (long-lived child where you stream stdout, custom env handling, etc.) AND you've thought through the error paths. If you must, the patterns to avoid:

```rust
// ❌ SILENT — exit code and stderr discarded.
let _ = tokio::process::Command::new("systemctl").args([...]).output().await;

// ❌ SILENT on Err — spawn failure lost, only success path is handled.
if let Ok(out) = tokio::process::Command::new("systemctl").args([...]).output().await {
    /* ... */
}

// ✅ LOGGED — spawn failure and non-zero exit always go to the journal.
nasty_common::cmd::try_run("systemctl", &["restart", "foo"]).await;
```

#### Spawned tasks — log errors before they vanish

A `tokio::spawn` block that produces a `Result` and discards it loses the failure forever. At minimum:

```rust
// ❌ SILENT
tokio::spawn(async move {
    do_thing_that_can_fail().await
});

// ✅ LOGGED
tokio::spawn(async move {
    if let Err(e) = do_thing_that_can_fail().await {
        warn!("background do_thing failed: {e}");
    }
});
```

If the function being spawned already logs internally (e.g. `apply_caddy_tls` from settings.rs), this isn't needed — but spawning a `JoinHandle` and never awaiting it loses task-level panics, so for important background work consider:

```rust
let h = tokio::spawn(async move { background_work().await });
tokio::spawn(async move {
    if let Err(e) = h.await {
        warn!("background_work task panicked / cancelled: {e}");
    }
});
```

#### Aggregate operations — don't collapse per-item errors

A loop that processes N items and returns one `Result<(), _>` or one `bool` hides which item failed. This was the exact #197 bug. Pattern:

```rust
// ❌ HIDES — caller sees "apply failed" with no idea which connection.
for conn in connections {
    apply_one(conn).await?;
}
Ok(())

// ✅ SURFACES — caller gets a per-item map of {id: error}.
let mut errors = HashMap::new();
for conn in connections {
    if let Err(e) = apply_one(&conn).await {
        warn!("apply {}: {e}", conn.id);
        errors.insert(conn.id.clone(), e.to_string());
    }
}
Ok(ApplyOutcome { errors })
```

The WebUI / RPC response should expose the per-item errors so the user can see *which* operation broke without having to ssh in and read journals.

#### Don't erase the source error

```rust
// ❌ STATIC — original error type and details lost.
some_op().await.map_err(|_| "operation failed".to_string())?;

// ✅ KEEPS the cause.
some_op().await.map_err(|e| format!("operation failed: {e}"))?;
```

### When silent discards are OK

There are a few legitimate cases where `let _ = ...` is fine — they're all "the failure literally doesn't matter":

- Cleanup paths where the process is exiting anyway (`tokio::fs::remove_file` of a pipe socket on shutdown).
- Channel sends where the receiver going away is the expected shutdown signal.

Even then, prefer `if let Err(e) = ... { tracing::trace!("…") }` — the `trace!` level keeps the journal quiet but leaves a breadcrumb if someone enables it.

### Enforcement

The standard CI run (`cargo clippy --workspace --all-targets --no-deps -- -D warnings`) will reject anything that violates the project's clippy config in `engine/clippy.toml`. New raw `tokio::process::Command::new` callsites are not enforced yet — they're being migrated as we touch the surrounding code. Don't introduce new ones; use the helpers above.
