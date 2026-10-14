# Performance diagnostics

Use **Settings → System tuning → Performance diagnostics** as an unscoped admin.
This feature is independent of usage telemetry. It never uploads reports.

1. Select **Capture details for 15 minutes** before reproducing a slow action.
2. Reproduce it in the same browser. A browser timeout does not cancel engine work;
   wait for a late response where possible.
3. Select **Preview report**. Review the summary and complete JSON, then download
   the reviewed snapshot. Explicitly attach it to an issue if you choose to share it.
4. Stop detailed capture or let it expire. **Clear history** clears engine history
   and this browser's timing history, not other browsers' histories.

Returning to Settings while capture is active automatically opens System tuning
and loads a fresh report with the complete preview expanded. Retained prior-engine
reports are also available even after capture stops.

## Collected information

The engine keeps a bounded in-memory ring of 2,048 structured timings. Ordinary
RPC backend timings and response conversion timings are always recorded locally.
Detailed mode adds command timing (only fixed executable categories), filesystem
discovery timing, and filesystem-list cache-lock waits. Command cancellation is
recorded without changing process execution/cancellation behavior. No diagnostic
scans or repair operations are started.

The browser keeps at most 256 request timings and watches at most 128 timed-out
requests for late replies. The downloaded report correlates these with backend
timings using report-local request labels. Unknown methods are not exported;
methods are allowlisted against the engine's RPC registry. Browser timings from
other tabs/clients are not available, and entries evicted on either side may no
longer correlate. No raw JSON-RPC request IDs or engine-instance identifiers are
included in the downloaded report.

Context includes engine/numeric kernel versions, power-of-two buckets for CPU
count, RAM, pool capacity and device counts, and numeric Linux pressure averages.
Pool context comes from the last ordinary filesystem listing, may be stale, is
limited to 128 pools, and uses sorted report-local labels. Boot phase names,
states and durations are included, but their error messages are not.

There are no raw logs, request parameters/results, command arguments/output,
paths, filenames, usernames, hostnames, addresses, UUIDs, serial numbers, or
credentials in the export. This is data minimization, not a guarantee of complete
anonymity: timing patterns, versions and size buckets can still reveal workload
characteristics. Review before sharing.

## API

All methods require an **unscoped Admin** session:

- `system.diagnostics.report`: structured preview, including temporary correlation
  metadata used by the WebUI. Use the WebUI download for report-local relabeling.
- `system.diagnostics.capture`, with `{"enabled": true|false}`: start a 15-minute
  detailed capture or stop it. Re-enabling restarts the 15-minute window.
- `system.diagnostics.capture_reboot`, with `{"enabled": true|false}`: opt in to
  retained checkpoints for 30 minutes, or stop checkpointing while retaining history.
- `system.diagnostics.clear`: stop capture and clear retained OS-disk history,
  engine timings, and cached pool context.

RPC responses include an additive `_timing` extension containing duration and
temporary engine-local correlation metadata. Existing clients may ignore it.
No existing request timeout, repair safety, or networking policy is changed.

## Across reboot

An unscoped admin can open Settings → Performance diagnostics and select
**Capture across reboot for 30 minutes**. This is separate from the existing
15-minute, in-memory detailed capture. It does not initiate a reboot.

1. Enable reboot capture before reproducing a slow operation or restarting.
2. Reboot normally if that is the operation being investigated.
3. After login, return to diagnostics and select **Preview report**.
4. Inspect `persistent_capture.previous_sessions` as well as the current
   timings and boot phases. Download only after reviewing the report.
5. Stop capture to retain the checkpoints, or **Clear history** to stop capture
   and remove both retained engine checkpoints and browser history.

The engine writes an initial checkpoint when enabled, then every five seconds
to `/var/lib/nasty/performance-diagnostics.json` on the OS filesystem. Atomic
replacement and fsync are used, with owner-only permissions. The file is capped
at 2 MiB, with at most three engine sessions (each timing ring has 2048 records).
Capture expires after 30 minutes, including time spent rebooting/offline.
History is retained for at most 24 hours; expired history is removed when the
engine is running or next starts. No extra data-pool scans are performed.

Checkpoints contain structured durations, outcomes, bucketed hardware/pool
context and boot-phase states, not raw journals, error text, request payloads,
paths, filenames, credentials, command arguments/output or hardware identities.
Session correlation uses a local random engine epoch; export replaces request
sequence numbers with report-local aliases and omits epochs. Previous sessions
are separate and never correlated with the current browser's requests.

## Interpretation and limits

- `received_to_backend` measures authentication, parsing and permission checks
  after the engine reads a request. It does **not** measure time the request
  waits unread behind earlier work on the sequential WebSocket connection.
- Backend/serialization/stage durations overlap; do not add them together.
- A browser timeout does not cancel or safely replay a server-side mutation.
- Periodic checkpoints may miss the last interval on abrupt process/power loss;
  no final shutdown checkpoint, kernel trace or crash dump is promised.
- Engine boot phases are captured, but firmware, kernel boot, early engine
  initialization and maintenance boots (where the engine is offline) are not.
- Browser timing history still disappears on page reload. The previous-engine
  section supplies server-side evidence, not missing browser timeout events.
- Disk/checkpoint failures are reported without path/output details and do not
  change storage recovery policy. Nothing is uploaded automatically.

This instrumentation diagnoses symptoms; it neither raises global RPC timeouts
nor changes request concurrency or storage mutation behavior.
