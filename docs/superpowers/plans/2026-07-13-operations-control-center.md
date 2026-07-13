# Operations Control Center Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Operations page show and control every bcachefs array operation across pools — scrub gains an always-present idle row with Start, evacuate is acknowledged when idle — and remove the duplicated scrub/reconcile controls from the per-filesystem diagnostics panel.

**Architecture:** The engine's `build_operations` aggregator emits an idle scrub row (with a new `start` control) per pool and one "no evacuation in progress" row when nothing is draining; the Operations page wires the `start` control to `fs.scrub.start`; the `BcachefsDiagnostics` scrub/reconcile tabs lose their action buttons and become read-only telemetry with a pointer to Operations.

**Tech Stack:** Rust (`nasty-engine`), Svelte 5 (`webui`).

## Global Constraints

- `Operation.control` gains one new value: **`"start"`** (for idle scrub). It's an additional string in an existing free-form field — not a schema break. Existing values (`cancel`/`pause`/`resume`/`none`) are unchanged.
- **Scrub** emits a row per mounted pool *always*: running → `state:"running"`, `control:"cancel"` (unchanged); idle → `state:"idle"`, `control:"start"`.
- **Evacuate**: per-device running rows (`control:"cancel"`) unchanged; when **no device on any pool** is evacuating, append exactly **one** row `kind:"evacuate"`, `fs:""`, `state:"idle"`, `control:"none"`, `detail:"No evacuation in progress"`.
- **Reconcile / copygc** aggregation is unchanged (already always-present per pool).
- Starting a scrub is **non-destructive** → no confirm dialog (unlike cancel). It calls `fs.scrub.start { name: <fs> }`.
- The diagnostics **scrub** and **reconcile** tabs keep their read-only telemetry; only the Start-scrub button and the reconcile enable/disable toggle are removed (replaced by a one-line pointer to Operations). The **usage / top / timestats** tabs are untouched.
- `fs.scrub.start` / `fs.scrub.cancel` are Admin-role (unchanged); the new Start control inherits the same central gating as the existing Cancel — no `is_operator_allowed`/registry changes in this feature.
- Verification before every Rust commit (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`. For WebUI (from `webui/`): `npm run check && npm test`.

---

## File Structure

- `engine/nasty-engine/src/router/system.rs` — **modify**: two pure helpers (`scrub_idle_detail`, `evacuate_idle_row`) + a `#[cfg(test)]` module for them; extend `build_operations` (idle scrub row, evacuate-idle row).
- `webui/src/routes/operations/+page.svelte` — **modify**: `act()` + `actionLabel()` handle the `start` control; Start button variant; intro copy.
- `webui/src/lib/components/BcachefsDiagnostics.svelte` — **modify**: remove the Start-scrub button and the reconcile enable/disable toggle (+ their now-unused handlers/state); add pointers to Operations.

---

## Task 1: Engine — idle scrub rows + evacuate-idle row

**Files:**
- Modify: `engine/nasty-engine/src/router/system.rs`
- Test: `engine/nasty-engine/src/router/system.rs` (`#[cfg(test)] mod operations_tests`)

**Interfaces:**
- Consumes: `nasty_system::Operation`; `ScrubStatus` (the type `state.filesystems.scrub_status()` returns — `nasty_storage::filesystem::ScrubStatus`, with `running: bool`, `last_run_at: Option<i64>`, `last_outcome: Option<ScrubOutcome>`; `ScrubOutcome` variants `Ok | Errors | Failed | Cancelled`).
- Produces:
  - `fn scrub_idle_detail(fs: &str, s: &ScrubStatus) -> String`
  - `fn evacuate_idle_row() -> nasty_system::Operation`
  - `build_operations` now always emits a scrub row per pool and one evacuate-idle row when nothing is evacuating.

- [ ] **Step 1: Write the failing tests**

Add at the bottom of `engine/nasty-engine/src/router/system.rs` (create the module if absent; import what it needs from `super`):

```rust
#[cfg(test)]
mod operations_tests {
    use super::{evacuate_idle_row, scrub_idle_detail};
    use nasty_storage::filesystem::{ScrubOutcome, ScrubStatus};

    fn status(last_run_at: Option<i64>, last_outcome: Option<ScrubOutcome>) -> ScrubStatus {
        ScrubStatus {
            running: false,
            started_at: None,
            progress_percent: None,
            last_run_at,
            last_duration_secs: None,
            last_outcome,
            last_output: None,
            raw: String::new(),
        }
    }

    #[test]
    fn scrub_idle_detail_never_run() {
        assert_eq!(scrub_idle_detail("tank", &status(None, None)), "Scrub tank — never run");
    }

    #[test]
    fn scrub_idle_detail_summarizes_last_outcome() {
        assert_eq!(
            scrub_idle_detail("tank", &status(Some(1_700_000_000), Some(ScrubOutcome::Ok))),
            "Scrub tank — last run clean"
        );
        assert_eq!(
            scrub_idle_detail("tank", &status(Some(1_700_000_000), Some(ScrubOutcome::Errors))),
            "Scrub tank — last run found errors"
        );
        assert_eq!(
            scrub_idle_detail("tank", &status(Some(1_700_000_000), Some(ScrubOutcome::Failed))),
            "Scrub tank — last run failed"
        );
        assert_eq!(
            scrub_idle_detail("tank", &status(Some(1_700_000_000), Some(ScrubOutcome::Cancelled))),
            "Scrub tank — last run cancelled"
        );
    }

    #[test]
    fn scrub_idle_detail_ran_but_no_outcome() {
        // last_run_at present but outcome missing (older state) → plain idle.
        assert_eq!(scrub_idle_detail("tank", &status(Some(1_700_000_000), None)), "Scrub tank — idle");
    }

    #[test]
    fn evacuate_idle_row_shape() {
        let r = evacuate_idle_row();
        assert_eq!(r.kind, "evacuate");
        assert_eq!(r.state, "idle");
        assert_eq!(r.control, "none");
        assert!(r.target.is_none());
        assert_eq!(r.detail, "No evacuation in progress");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run (from `engine/`): `cargo test -p nasty-engine operations_tests`
Expected: FAIL — `scrub_idle_detail` / `evacuate_idle_row` not found.

- [ ] **Step 3: Add the two pure helpers**

Near `build_operations` in `system.rs` (add `use nasty_storage::filesystem::ScrubStatus;` if not already imported for the helper signature — the aggregator already uses the type via `state.filesystems.scrub_status()`):

```rust
/// Detail line for an idle (not-running) scrub row, summarizing the most
/// recent completed run when the engine has it.
fn scrub_idle_detail(fs: &str, s: &nasty_storage::filesystem::ScrubStatus) -> String {
    use nasty_storage::filesystem::ScrubOutcome;
    match (s.last_run_at, s.last_outcome) {
        (None, _) => format!("Scrub {fs} — never run"),
        (Some(_), Some(ScrubOutcome::Ok)) => format!("Scrub {fs} — last run clean"),
        (Some(_), Some(ScrubOutcome::Errors)) => format!("Scrub {fs} — last run found errors"),
        (Some(_), Some(ScrubOutcome::Failed)) => format!("Scrub {fs} — last run failed"),
        (Some(_), Some(ScrubOutcome::Cancelled)) => format!("Scrub {fs} — last run cancelled"),
        (Some(_), None) => format!("Scrub {fs} — idle"),
    }
}

/// The single informational row shown when no device is evacuating, so the
/// evacuate operation type is acknowledged rather than silently absent.
/// Evacuations are started from the Disks device-removal flow.
fn evacuate_idle_row() -> nasty_system::Operation {
    nasty_system::Operation {
        kind: "evacuate".into(),
        fs: String::new(),
        target: None,
        state: "idle".into(),
        progress_percent: None,
        detail: "No evacuation in progress".into(),
        control: "none".into(),
    }
}
```

Note: `ScrubOutcome` derives `Copy` (it's a `#[derive(... Copy ...)]` enum), so `s.last_outcome` in the `match (s.last_run_at, s.last_outcome)` tuple copies rather than moves. If the compiler complains it isn't `Copy`, match on `s.last_outcome.as_ref()` and adjust the arms to `Some(&ScrubOutcome::Ok)` etc.

- [ ] **Step 4: Run to verify they pass**

Run (from `engine/`): `cargo test -p nasty-engine operations_tests`
Expected: PASS.

- [ ] **Step 5: Wire the helpers into `build_operations`**

Two edits in `build_operations`:

(a) **Scrub — emit an idle row when not running.** Find the scrub block:

```rust
        if let Ok(scrub) = state.filesystems.scrub_status(&fs.name).await
            && scrub.running
        {
            // ... existing running-row push ...
        }
```

Change it to also handle the idle case:

```rust
        if let Ok(scrub) = state.filesystems.scrub_status(&fs.name).await {
            if scrub.running {
                let mut detail = match scrub.progress_percent {
                    Some(p) => format!("Scrub {} — {p:.0}%", fs.name),
                    None => format!("Scrub {}", fs.name),
                };
                if let Some((seen, _)) = moved("scrub").filter(|&(s, _)| s > 0) {
                    detail += &format!(" ({} scanned)", human_bytes(seen));
                }
                ops.push(nasty_system::Operation {
                    kind: "scrub".into(),
                    fs: fs.name.clone(),
                    target: None,
                    state: "running".into(),
                    progress_percent: scrub.progress_percent,
                    detail,
                    control: "cancel".into(),
                });
            } else {
                ops.push(nasty_system::Operation {
                    kind: "scrub".into(),
                    fs: fs.name.clone(),
                    target: None,
                    state: "idle".into(),
                    progress_percent: None,
                    detail: scrub_idle_detail(&fs.name, &scrub),
                    control: "start".into(),
                });
            }
        }
```

(The running branch is the same code as before, just moved inside `if scrub.running`.)

(b) **Evacuate — one idle row when nothing is draining.** Add `let mut any_evacuating = false;` immediately before the `for fs in &filesystems {` loop. Inside the existing per-device evacuate push (where `dev.state.as_deref() == Some("evacuating")`), set `any_evacuating = true;` right after `ops.push(...)`. Then, after the `for fs` loop closes and before `ops` is returned, add:

```rust
    if !any_evacuating {
        ops.push(evacuate_idle_row());
    }
    ops
```

- [ ] **Step 6: Update the `Operation.control` doc comment**

In `engine/nasty-system/src/lib.rs`, the `control` field doc currently lists `"cancel" | "pause" | "resume" | "none"`. Add `"start"`:

```rust
    /// Action the UI offers: "start" (idle scrub) | "cancel"
    /// (scrub/evacuate) | "pause" | "resume" (reconcile/copygc) | "none".
    pub control: String,
```

- [ ] **Step 7: Full verification + commit**

Run (from `engine/`): `cargo fmt --check && cargo clippy --workspace --all-targets --no-deps -- -D warnings && cargo test`
Expected: clean; `operations_tests` pass.

```bash
cd engine && cargo fmt
git add nasty-engine/src/router/system.rs nasty-system/src/lib.rs
git commit -m "operations: always list idle scrub (start) + acknowledge idle evacuate"
```

---

## Task 2: WebUI — Operations page renders & wires the new states

**Files:**
- Modify: `webui/src/routes/operations/+page.svelte`

**Interfaces:**
- Consumes: the engine list from Task 1 (`control:"start"` scrub rows; the `control:"none"` evacuate-idle row); `client.call('fs.scrub.start', { name })`.

- [ ] **Step 1: Handle the `start` control in `act()`**

In `act()`, the confirm block currently only fires for `cancel`; leave it (start needs no confirm). Replace the `method` and `verb` selection to add scrub-start:

Change the `method` expression's scrub arm from `'fs.scrub.cancel'` to branch on control:

```ts
		const method =
			op.kind === 'scrub'
				? op.control === 'start'
					? 'fs.scrub.start'
					: 'fs.scrub.cancel'
				: op.kind === 'evacuate'
					? 'fs.device.evacuate.cancel'
					: op.kind === 'reconcile'
						? op.control === 'resume'
							? 'fs.reconcile.enable'
							: 'fs.reconcile.disable'
						: op.control === 'resume'
							? 'fs.copygc.enable'
							: 'fs.copygc.disable';
```

And the toast `verb`:

```ts
		const verb =
			op.control === 'start'
				? 'started'
				: op.control === 'cancel'
					? 'cancelled'
					: op.control === 'resume'
						? 'resumed'
						: 'paused';
```

(`params` is already `{ name: op.fs }` for scrub — correct for `fs.scrub.start`.)

- [ ] **Step 2: Label + button variant for `start`**

In `actionLabel`, add `start`:

```ts
	function actionLabel(op: Operation): string {
		return (
			{ start: 'Start', cancel: 'Cancel', pause: 'Pause', resume: 'Resume' }[op.control] ?? ''
		);
	}
```

In the row's `<Button>`, make Start the primary variant (cancel stays destructive, others outline):

```svelte
							<Button
								variant={op.control === 'cancel' ? 'destructive' : op.control === 'start' ? 'default' : 'outline'}
								size="sm"
								disabled={busy === opKey(op)}
								onclick={() => act(op)}
							>
```

(The existing `{#if op.control !== 'none'}` guard already hides the button on the evacuate-idle row.)

- [ ] **Step 3: Refresh the intro copy**

Replace the intro `<span>` text:

```svelte
		<span>Live array operations across your pools — start or cancel a scrub, pause or resume
		background reconcile and copy-GC, and watch evacuations in progress.</span>
```

- [ ] **Step 4: Check + test + commit**

Run (from `webui/`): `npm run check && npm test`
Expected: 0 errors/warnings; suite passes.

```bash
git add webui/src/routes/operations/+page.svelte
git commit -m "webui: Operations page can start scrubs; shows idle scrub + evacuate rows"
```

---

## Task 3: WebUI — trim diagnostics scrub/reconcile tabs to read-only

**Files:**
- Modify: `webui/src/lib/components/BcachefsDiagnostics.svelte`

**Interfaces:**
- Consumes: nothing new — this removes controls that now live on the Operations page.

- [ ] **Step 1: Remove the Start-scrub control, keep the scrub telemetry**

In the **scrub tab** markup, remove the button that triggers `startScrub()` (the "Start scrub" / "Run scrub" button) and replace it with a muted pointer line:

```svelte
			<p class="text-xs text-muted-foreground">Run scrubs from the <a href="/operations" class="underline">Operations</a> page.</p>
```

Keep every read-only element in that tab (last-run outcome, progress mirror, `scrubFull` summary, status text). Then delete the now-unused `startScrub()` function. If removing it leaves other symbols unused (e.g. a `scrubRunning`/`scrubLoading` flag only the button read), delete exactly those that `npm run check` reports as unused — do not remove state still read by the retained telemetry (`scrubFull`, `scrubOutput`, the poller).

- [ ] **Step 2: Remove the reconcile toggle, keep the reconcile status**

In the **reconcile tab** markup, remove the enable/disable button that calls `toggleReconcile()` and replace it with:

```svelte
			<p class="text-xs text-muted-foreground">Pause or resume reconcile from the <a href="/operations" class="underline">Operations</a> page.</p>
```

Keep the reconcile status output + auto-refresh toggle (read-only telemetry). Delete the now-unused `toggleReconcile()` function and the `reconcileToggling` state. Keep `reconcileEnabled` only if it's still read to render status text; if it becomes unused after the button is gone, delete it too — resolve exactly what `npm run check` flags.

- [ ] **Step 3: Leave usage / top / timestats untouched**

Confirm no edits to those tabs.

- [ ] **Step 4: Check + test + commit**

Run (from `webui/`): `npm run check && npm test`
Expected: 0 errors/warnings (no unused-symbol complaints); suite passes.

```bash
git add webui/src/lib/components/BcachefsDiagnostics.svelte
git commit -m "webui: diagnostics scrub/reconcile tabs are read-only (controls live in Operations)"
```

---

## Self-Review

**Spec coverage** (against `docs/operations-control-center.md`):
- Always-present idle scrub row + Start → Task 1 (engine emit) + Task 2 (render/wire).
- Evacuate acknowledged when idle (one "no evacuation in progress" row) → Task 1.
- reconcile/copygc unchanged → not touched (correct).
- New `control:"start"` value + doc → Task 1.
- Diagnostics scrub/reconcile → read-only with pointer; usage/top/timestats untouched → Task 3.
- Roles (scrub start/cancel Admin; telemetry visible to ReadOnly; no allowlist/registry change) → inherent (no gating code touched; the Start control reuses the existing Admin-gated `fs.scrub.start`).
- Error handling (scrub start failure via toast, poll reconciles; idle-detail fallback; evacuate row display-only) → Task 2 uses `withToast`; Task 1 helper has the `(Some, None) → idle` fallback tested.
- Testing: pure helpers unit-tested (Task 1); WebUI via `npm run check`/`test` (Tasks 2–3); running-scrub/evacuate paths unchanged (Task 1 keeps the running branch verbatim).

**Placeholder scan:** No TBD/TODO. The two "delete exactly what `npm run check` reports as unused" notes (Task 3) are concrete instructions with a named check, not deferred work — unused-symbol resolution genuinely depends on what the component reads after the buttons are removed, and the check is the deterministic arbiter.

**Type consistency:** `scrub_idle_detail(&str, &ScrubStatus) -> String` and `evacuate_idle_row() -> Operation` are named identically in their defining task and the tests. `control:"start"` is produced in Task 1 and consumed in Task 2 (`act()` method/verb, `actionLabel`, button variant) with the exact same string. `Operation` field names (`kind/fs/target/state/progress_percent/detail/control`) match the struct in `nasty-system/src/lib.rs`. WebUI method names (`fs.scrub.start`, `fs.scrub.cancel`, `fs.device.evacuate.cancel`, `fs.reconcile.*`, `fs.copygc.*`) match the existing `act()` dispatch.
