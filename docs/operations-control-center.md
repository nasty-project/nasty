# Operations page: the array-operations control center

Make the **Operations** page the single, complete place to see and control
bcachefs array operations across all pools — and stop duplicating those
controls in the per-filesystem diagnostics panel.

## Problem

The Operations page (`system.operations.list` → `build_operations`) today
lists only:

- **reconcile** and **copygc** — always, because they're pausable background
  jobs (idle/active/paused, with Pause/Resume);
- **scrub** and **evacuate** — *only while running* (with Cancel).

So on an idle box you see reconcile + copygc and nothing else, which reads as
"scrub and evacuation are missing." You also can't *start* a scrub from here —
that lives in the Filesystems → diagnostics **scrub tab** — and the diagnostics
**reconcile tab** carries a second enable/disable toggle that duplicates
Operations' Pause/Resume. Two places to control the same jobs, and a
monitoring page that hides half the operations it names.

## Goal

Operations becomes the control center for pool-level array operations:
every operation is *visible* (never silently absent), and everything that can
sensibly be controlled from one cross-pool place is controlled here. The
per-filesystem diagnostics panel drops the duplicated controls and becomes
read-only telemetry with a pointer to Operations.

## Scope

**In scope:**
- Always list a **scrub** row per mounted pool — idle (with a **Start**
  action) or running (Cancel), instead of only when running.
- Acknowledge **evacuate** even when idle: a single informational
  "no evacuation in progress" row, plus the running per-device rows (Cancel)
  as today.
- Trim the diagnostics **scrub** and **reconcile** tabs to read-only
  telemetry (drop Start-scrub and the reconcile enable/disable toggle);
  point to Operations for the controls.

**Out of scope:**
- Starting an **evacuation** from Operations. Evacuation drains a specific
  device to remove it, so it stays initiated from the Disks device-removal
  flow (where you choose the device and its fate); Operations only monitors
  and cancels a running one.
- reconcile/copygc **start** actions — they're always-on background jobs;
  Pause/Resume is the only control and already exists.
- The diagnostics **usage / top / timestats** tabs — pure per-fs telemetry,
  no Operations overlap, untouched.

## Design

### 1. `build_operations` (engine) — complete the list

`build_operations` in `engine/nasty-engine/src/router/system.rs` loops over
mounted filesystems. Changes:

- **Scrub — always emit a row per pool.** When `scrub_status(fs).running`,
  emit the existing running row (state `running`, `control: "cancel"`,
  progress). Otherwise emit an **idle** row: state `idle`, a new
  `control: "start"`, and a `detail` summarizing the last run when the engine
  exposes it (e.g. "Scrub · <fs> — last run clean" / "never run"; fall back to
  "Scrub · <fs> — idle" when no history is available). No behavior change to
  the running case.
- **Reconcile / copygc — unchanged.** They already emit a per-pool row every
  time (idle/active/paused, Pause/Resume).
- **Evacuate — acknowledge idle once.** Keep the per-device running rows
  (`control: "cancel"`) exactly as now. After the per-filesystem loop, if
  **no device on any pool** is evacuating, append **one** informational row:
  `kind: "evacuate"`, `fs: ""` (or omitted), `state: "idle"`,
  `control: "none"`, `detail: "No evacuation in progress"`. A device
  evacuation is still started from the Disks page; this row exists only so
  the operation type is never invisible.

`Operation.control` gains the value **`"start"`** (documented in its enum
comment alongside `cancel`/`pause`/`resume`/`none`). No schema-breaking
change — it's an additional string value in an existing free-form field.

### 2. Operations page (WebUI) — render + wire the new states

`webui/src/routes/operations/+page.svelte`:

- Render the **idle scrub** rows and the **evacuate "none in progress"** row
  the engine now returns; the existing per-row layout (label, detail, state
  chip, progress bar, control button) already generalizes.
- Map the new `control === "start"` to a **Start** button that calls
  `fs.scrub.start { name: op.fs }` (with a short confirm, matching the tone of
  the existing cancel confirm), then refreshes. `control === "none"` renders no
  button (the evacuate idle row; optionally a muted "start from Disks when
  removing a device" hint).
- The empty-state text ("Nothing running…") is no longer reachable in normal
  operation (there's always at least reconcile/copygc/scrub per pool) — keep a
  minimal fallback for the no-mounted-filesystem case.

### 3. Diagnostics panel (WebUI) — controls out, telemetry stays

`webui/src/lib/components/BcachefsDiagnostics.svelte`:

- **Scrub tab:** keep the read-only readout (last-run outcome, progress
  mirror, status). Remove the **Start scrub** button; replace with a one-line
  pointer ("Run scrubs from the Operations page").
- **Reconcile tab:** keep the status output + auto-refresh. Remove the
  **enable/disable** toggle; same one-line pointer ("Pause/resume reconcile
  from the Operations page").
- **usage / top / timestats tabs:** unchanged.

No capability is lost — the scrub launch and reconcile toggle move to
Operations, they aren't deleted.

## Roles / gating

`fs.scrub.start` and `fs.scrub.cancel` are **Admin** (unchanged); the
reconcile/copygc toggles are what they are today. The Operations page is
readable by any role (`system.operations.list` is a read), so a non-admin
still *sees* all operations and their live state — the **control buttons** are
gated by the same central role check that already governs the existing
Cancel/Pause/Resume actions. The new Start control inherits that gating with no
new surface. Moving the diagnostics controls to Operations, and leaving the
diagnostics tabs read-only, means a ReadOnly user can still inspect scrub/
reconcile telemetry per filesystem — an improvement, not a regression.

## Error handling

- A failed `fs.scrub.start` (e.g. a scrub already running, or the pool
  unmounted mid-click) surfaces via the existing toast path, same as the
  current cancel/pause/resume error handling; the poll loop then reconciles
  the row's state on its next tick.
- If `scrub_status` or the last-run history is unavailable for a pool, the
  idle scrub row falls back to the plain "idle" detail rather than omitting
  the row — the row's *presence* is the point.
- The evacuate idle row is display-only; it has no failure mode.

## Testing

- **Unit (engine):** extract the idle-scrub `detail` formatting and the
  "emit one evacuate-idle row iff nothing is evacuating" decision as pure
  helpers and test them (never-run vs last-clean vs unavailable-history; zero
  evacuating devices → one row, ≥1 evacuating → running rows and no idle row).
  The full `build_operations` aggregation stays integration territory (it
  reads `filesystems.list` / `scrub_status` / `moving_ctxts`), as it is today.
- **WebUI:** `npm run check` + existing suite; the page has no component-test
  harness, so the render is validated against a real engine. Manually verify:
  idle box shows scrub (Start) + reconcile + copygc per pool + one evacuate
  "none in progress" row; starting a scrub flips that pool's row to running
  with a progress bar; the diagnostics scrub/reconcile tabs are read-only with
  the pointer.
- **Regression:** confirm a running scrub / active evacuation still render and
  cancel exactly as before (the running paths are unchanged).

## Follow-ups (not now)

- A per-pool scrub schedule ("scrub weekly") would fit naturally as a control
  on the idle scrub row later.
- If Operations grows more per-pool actions, consider grouping rows by pool.
