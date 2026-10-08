# Performance diagnostics

Use **Settings → General → Performance diagnostics** as an unscoped admin.
This feature is independent of usage telemetry. It never uploads reports.

1. Select **Capture details for 15 minutes** before reproducing a slow action.
2. Reproduce it in the same browser. A browser timeout does not cancel engine work;
   wait for a late response where possible.
3. Select **Preview report**. Review the summary and complete JSON, then download
   the reviewed snapshot. Explicitly attach it to an issue if you choose to share it.
4. Stop detailed capture or let it expire. **Clear history** clears engine history
   and this browser's timing history, not other browsers' histories.

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

## Interpreting timings

Summaries show count, cumulative duration, maximum, p50 and p95, sorted by
cumulative duration. Stage durations overlap with backend durations; do not add
them together. Backend duration excludes socket queueing. The difference between
client and backend duration includes transport, socket queueing and browser
scheduling; it is **not** a measured lock wait.

Detailed task-local context covers awaited subprocess helpers; independently
spawned tasks may not inherit it. This is targeted instrumentation, not a profiler
of every lock or function. Pressure is observed when the report is requested,
not sampled continuously alongside every operation.

History is lost on engine restart or browser reload. This first version does not
persist shutdown/unmount/reboot observations across boots, cannot diagnose a
completely unresponsive engine, and is unavailable while the engine is disabled
in storage maintenance. Durable systemd-level lifecycle capture is a follow-up.

## API

All three methods require an **unscoped Admin** session:

- `system.diagnostics.report`: structured preview, including temporary correlation
  metadata used by the WebUI. Use the WebUI download for report-local relabeling.
- `system.diagnostics.capture`, with `{"enabled": true|false}`: start a 15-minute
  detailed capture or stop it. Re-enabling restarts the 15-minute window.
- `system.diagnostics.clear`: clear engine timing and cached pool context.

RPC responses include an additive `_timing` extension containing duration and
temporary engine-local correlation metadata. Existing clients may ignore it.
No existing request timeout, repair safety, or networking policy is changed.
