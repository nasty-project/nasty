# Changelog

## v0.0.9 — 2026-05-29

> **This is the Secure Boot + Passkeys release.** Boxes can opt into a Secure Boot–enforcing boot chain (lanzaboote) with a guided enrollment ceremony, TPM2-sealed bcachefs encryption keys, and WebAuthn sign-in via passkeys. The v0.0.8 nginx → Caddy migration scaffolding has been removed — boxes upgrading from v0.0.7 should pass through v0.0.8 first.

### Headline changes

- **Per-box Secure Boot opt-in.** New `nasty.secureBoot.enable` flag wires in lanzaboote as a conditionally-imported sub-module so non-SB boxes pay zero cost. The Hardware page gains a readiness checklist (RPC + UI) showing whether the box can host SB, and an experimental enrollment ceremony wizard that walks operators through Phase 1/2 with a Rebuild button and accurate Abort copy. SB state is read via `bootctl` (single source of truth, replacing the earlier sbctl probe), surfaced in the Hardware page, and `kexec` is disabled once a box is SB-enrolled. Firmware-apply gracefully refuses with a documented reason when SB is enforcing (lanzaboote#591 incompatibility). Documented end-to-end in ADR 0001. (#323, #324, #325, #326, #331, #333, #335)

- **WebAuthn / passkeys for sign-in (#289).** Three-PR landing: credential **registration and self-management** so any user can manage their own keys (#327), **sign-in** alongside password (#328), and **safeguards** — operators can require a fallback factor on accounts that have keys, and admins can reset another user's WebAuthn credentials (#329). Origin precheck so HTTP / wrong-host / direct-IP access fails legibly instead of erroring out of WebAuthn's own validator. WebUI: `/account` folded into Access Control; Users page split into **Users & Groups / Tokens & Keys / Single Sign-On** tabs.

- **TPM2-sealed bcachefs encryption keys (#102).** `storage: seal bcachefs encryption keys to TPM2` lands the storage half (#287); `webui: bind/unbind encryption keys to TPM2` lands the UI (#290); `fs.create: optional bind_to_tpm flag` lets operators seal in one step at filesystem creation rather than create-then-bind (#320). Hardware page now reports TPM2 presence, vendor info via `tpm2_getcap` (#284), and capability detection (#283).

- **Reliable upgrade flow end-to-end.** Engine self-reports its own build commit as "current version" — no more `/run/booted-system` lag where it pointed at the previous closure post-activation, looping the "Upgrade available" prompt (#315). `version_switch` runs the wrapper-shape migration *first* so legacy wrapper-flakes get migrated atomically before any downstream version logic runs (#308, #313). Default upgrade flow refreshes only the `nasty` input — won't drag along uncommitted nixpkgs / bcachefs-tools bumps (#293). Update page shows **all three flake inputs** with the GitHub lookup error surfaced if any one fails (#294). Half-applied upgrades bubble up through the WebUI instead of swallowing failures into the spinner (#292). Build-dir override collapsed behind a "recovery drawer" so it's there when you need it but invisible in the happy path; opt-in bcachefs build-dir spillover for small-rootfs installs (#295).

- **Boot reliability.** Per-phase boot timeouts and budgets sized to realistic worst case so one hung phase doesn't take the whole engine down (#300). New `system.boot_status` RPC and `/api/boot_status` REST endpoint (#301); WebUI gets a booting overlay during startup and a post-boot health banner (#302). Encrypted-FS boot mount no longer hangs — bcachefs is probed for encryption before unlock (#305), and the storage layer distinguishes encryption-required from key-incorrect failure modes (#297).

### Authentication & users

- WebAuthn credential registration and self-management (#289 PR #1, #327).
- WebAuthn sign-in alongside password (#289 PR #2, #328).
- WebAuthn safeguards — operator-required fallback factor; admin-side credential reset for locked-out users (#289 PR #3, #329).
- Origin precheck so IP / wrong-host / HTTP access fails legibly instead of breaking inside webauthn-rs's validator.
- `/account` page folded into Access Control; security-keys controls live with the rest of access control.
- Users page split into **Users & Groups**, **Tokens & Keys**, and **Single Sign-On** tabs.

### Secure Boot

- `nasty.secureBoot.enable` toggle — per-box opt-in. Lanzaboote module imported conditionally so non-SB closures stay lean (#325).
- Readiness probe (`system.secure_boot.readiness`) + Hardware-page checklist (#326).
- Enrollment ceremony wizard (experimental, marked as such in the UI) with Rebuild button and accurate Abort copy (#331).
- SB state read via `bootctl` everywhere (replaces the earlier sbctl probe) (#323).
- `kexec` disabled on SB-enrolled boxes; documented in the SB glossary.
- Firmware apply refuses cleanly under enforcing SB with an operator-readable reason — engine-owned string consistent across banner, tooltip, and defensive RPC refusal (#333).
- Inline manual-unenroll recipe and glossary entries documented.
- Documented end-to-end in ADR 0001 (#324).
- Role classification: `system.secure_boot.readiness` correctly marked read-only so the Hardware page renders for Operator and ReadOnly users (#335).

### TPM2

- Capability detection on the Hardware page (#283).
- Vendor info populated from `tpm2_getcap` (#284).
- bcachefs encryption keys sealable to TPM2 (#102, #287).
- `fs.create` accepts `bind_to_tpm` for one-step sealing at filesystem creation (#320).
- Bind / unbind from the WebUI Filesystems page (#290).
- `nasty-engine` has `keyutils` on PATH and the TPM2 TCTI pinned (#296).

### Update flow

- Engine self-reports its build commit as current version — no `/run/booted-system` lag (#315).
- `version_switch` runs the wrapper-shape migration first (#308, #313).
- Default flow refreshes only the `nasty` input (#293); the three-input view explains exactly what's being touched (#294).
- Per-input GitHub lookup errors surface individually on the Update page (#294).
- Half-applied upgrades bubble up through the WebUI (#292).
- Build-dir override behind a recovery drawer; opt-in bcachefs build-dir spillover for small-rootfs installs (#295).
- `wrapper-flake` template: `nixpkgs` + `bcachefs-tools` follow `nasty` (#304); canonical no-placeholder shape; explicit forward-compat shim that keeps the `@BCACHEFS_TOOLS_REF@` mention even when the active template doesn't use it (#317).
- `version_switch` preserves the operator's bcachefs pin across rebootstrap.
- bcachefs-tools back to an independently-pinned flake input; UI hides nixpkgs from the "what to bump" surface — it follows automatically (#312).
- `nasty-sync` CLI: engine-bypass recovery + state inspection (#314); `-r` rescue mode + auto-detach when run from a WebUI terminal (#316); `-n <ref>` flag for switching `nasty.url`'s tracked ref (#318).

### Boot reliability

- Per-phase boot timeouts and worst-case budgets (#300).
- `system.boot_status` RPC + `/api/boot_status` REST endpoint (#301).
- WebUI booting overlay + post-boot health banner (#302).
- Encrypted-FS boot mount hang fixed; login-failure vs encryption-missing distinguished (#297).
- bcachefs encryption probed before unlock attempt (#305).
- `nasty-engine`: `keyutils` on PATH; TPM2 TCTI pinned (#296).
- Unmount preserves tuned per-FS mount options so remount doesn't silently reset them (#298).

### Telemetry

- Engine reports `version` (semver), `commit` (short SHA), `vms`, `apps`, and `arch` alongside the existing drives / capacity / used fields (#334).
- Worker schema, validators, and dashboard updated; new "Versions" and "Architecture" breakdown panels group commits under their semver as `<version>+`.
- Telemetry rows retained indefinitely — instances going silent no longer wipe their history at the 30-day mark, so historical peaks remain visible on the chart.

### VM

- Support multiple CD-ROMs per VM (#285, #286).

### Operational / DX

- Aggressive RPC reconnect during Update — the spinner clears in seconds instead of waiting on the default backoff (#322).
- WS reconnect recovers from silent TLS-reject after self-signed leaf rotation (#309).
- `system.log.level`: live log filter reported back to the WebUI.
- Caddy install no longer tries to install its internal-CA root into the OS trust store.
- systemd-boot menu label uses `nasty-version`.
- Router classifies `*.status` RPCs as read-only — avoids the refresh-loop on event-bus broadcasts that used to keep `isBusy()` blinking on the Filesystems page (#306).
- memtest86+ as a systemd-boot menu entry (x86 only — aarch64 boxes skip the entry, no rebuild breakage) (#330, #332).

### Removed

- **nginx → Caddy migration paths.** v0.0.8's reverse-proxy migration scaffolding is gone — both the engine cutover code and the lego → Caddy-ACME migration (#307). Boxes on v0.0.7 should pass through v0.0.8 first.

### Build / CI

- Slow Nix jobs sped up with `paths-filter` + Magic Nix Cache (#311).
- Wrapper-flake template included in engine Nix `src`; gated on Nix engine build pre-merge (#310).
- Installer-template check inverted to enforce the no-placeholder shape (#317).
- `flake.nix` overlay sends an identifying User-Agent on every `fetchurl` so `crates.io`'s enforcement of its [crawler policy](https://crates.io/policies) doesn't block PRs that introduce new crate versions (#337). Temporary until [NixOS/nixpkgs#512735](https://github.com/NixOS/nixpkgs/pull/512735) propagates and `importCargoLock` gets the same fix.
- Weekly nixpkgs bump (#319).
- Workspace patch refreshes: `http 1.4.0 → 1.4.1`, `jiff 0.2.24 → 0.2.27` (#336).
- `nasty-engine` derivation: `pkg-config` + `openssl` added (transitively pulled by `webauthn-rs` via `openssl-sys`).

## v0.0.8 — 2026-05-22

> **This is the nginx → Caddy migration release.** The reverse proxy and TLS terminator under the WebUI moved from nginx to Caddy. ACME issuance is now driven directly through Caddy (lego dropped), per-app ingress applies at runtime via Caddy's admin API, and the v0.0.7 NetworkManager compatibility scaffolding has been removed — boxes upgrading from 0.0.7 should be reconciled before jumping. Anything still on 0.0.6 or earlier should pass through 0.0.7 first.

### Headline changes

- **Caddy replaces nginx as the reverse proxy and TLS terminator.** App ingress routes apply through Caddy's admin API at install / remove time — config changes take effect in-process with no file rewrite and no reload. TLS automation is one atomic admin-API PATCH per change, so per-host issuance state shows up live on the TLS page.

- **Per-app subdomain ingress (V1 of #99).** Apps can now be served at `app.example.com` instead of (or alongside) `/apps/<name>/`. Subdomain mode is selectable at install time and editable later, conflicts are detected before submit, and ingress-incompatible apps (whose absolute-path assets break path-prefix mode) auto-detect themselves and surface a clear reason in the install UI.

- **Self-signed certs now cover both `nasty.local` and the box's LAN / Tailscale IPs.** Direct-IP HTTPS (`https://10.x.x.x`) validates the cert against the IP directly — only the "untrusted CA" warning remains, which clears once you import Caddy's root via the **Download CA Root** button on the TLS page. Unknown SNI (tailnet `*.ts.net` names, anything not on the cert) falls back cleanly to the internal cert.

- **Files page learned copy, move, and bulk actions** (#88). Per-row Copy / Move icons + multi-select bulk action bar (Copy / Move / Delete) using the existing PathPicker. The same dialog handles files and directories regardless of which bcachefs pool the destination lives on.

- **NetworkManager compatibility scaffolding from v0.0.7 has been removed.** The legacy networking layer, the one-shot migration cutover, and the Phase-X comments are gone. A clean reconcile of orphan interfaces + NM profiles runs at startup, per-connection NM apply errors surface individually in the UI, and DBus type encoding for MAC / DNS fields aligns with what NetworkManager expects.

### TLS / reverse proxy

- ACME automation driven directly through Caddy — lego dropped, the entire TLS pipeline lives in one process.
- Nine DNS-01 plugins compiled in (Cloudflare, Route 53, Hetzner, Linode, Porkbun, Namecheap, DuckDNS, deSEC, RFC2136); per-provider directive emitter on the engine side.
- DNS-01 challenge knobs in Settings → TLS: `propagation_delay` (default 30s) and external resolvers (default 1.1.1.1, 8.8.8.8) — useful for split-horizon DNS, restricted egress, or providers with aggressive negative-TTL caching.
- Per-host issuance state on the TLS page: per managed name, issuing / active / failed / pending with the verbatim Caddy log line on failure.
- Cert directory polled after admin-API push, so the UI flips to Active as soon as Caddy lands the cert.
- WebUI exposes Caddy's local-CA root via a Download CA Root button — import once on each client to trust every per-name cert the internal CA issues.
- Caddy version pinned at build time for reproducible upgrades.

### Apps & ingress

- New **Ingress overview page** — every Caddy route in one place (host / path / catch-all, handler kind, upstream, per-row cert status for host-match routes).
- "Subdomain" menu is always present at install time; subdomain ingress survives reboots even for apps whose path-prefix mode was auto-disabled.
- Subdomain conflict detection before submit.
- Compose apps persist `ingress_subdomain` reliably across restarts (#247).
- Auto-detect apps whose absolute-path assets break path-prefix ingress; engine sets `proxy_disabled_reason` with a human-readable explanation, honoured on reconcile after restart.
- Curated sub-path recipes for Grafana and Vaultwarden — known-working env presets at install time.
- Idle-poll `apps.list` so containers crashing or installs from another tab show up without a refresh.
- Image inspection fetches registry tokens for ghcr.io and quay.io alongside Docker Hub.
- Docker-paste button on the Apps install form (paste a `docker run …` and the form fills itself out).
- Treat docker named volumes as auto-managed; tag image-default env vars so Edit can grey them out and only highlight values the operator set explicitly.
- Shell button surfaces `exec_command` errors directly when invocation fails.
- App install / remove robustness: real-world fixes from the Haze launch — better error reporting, idempotent re-install paths.
- Live per-app `apps.stats` rewritten for one Docker round-trip per frame; the Apps page renders stats instantly on load.
- Deploy WS close verifies app state before reporting failure — transient blips during `docker create_container` resolve correctly once the container reports up (#208).
- Port form on the Create/Edit dialog reads `Name | Exposed | Internal` left-to-right, matching `docker run -p HOST:CONTAINER` and every other UI in the ecosystem (#271).

### Files

- **Copy, Move, and Bulk actions** on the Files page (#88). Per-row Copy / Move icons, sticky bulk action bar (Copy / Move / Delete / Clear) when one or more rows are selected, select-all checkbox with indeterminate state.
- Cross-filesystem copy works natively — operators can move data between bcachefs pools mounted under `/fs` without dropping to a shell.

### Subvolumes

- **Per-row Usage column engine-side** (closes #81) — one snapshot of who owns what (NFS, SMB, iSCSI, NVMe-oF, apps, VMs, backup jobs) per subvolume, batched in a single RPC.
- **Cascade-delete dialog** — deleting a subvolume that backs an iSCSI target / NFS share / SMB share / NVMe-oF subsystem now lists what's in use and offers a single "Delete subvolume + N dependents" button. Apps / VMs / backups are surfaced with a direct link to their lifecycle page so cleanup stays explicit.
- Detail pane surfaces Apps and VMs as subvolume consumers — same dependency tree the Usage column reads.

### UPS / NUT

- **Remote NUT server mode.** NASty can now monitor a UPS attached to a different box (Synology, another NASty, a standalone NUT server) over the network — no USB-attached UPS required on the appliance itself.

### Networking

- v0.0.7's compatibility scaffolding removed: legacy networking layer gone, one-shot migration cutover gone, stale Phase-X / cutover comments stripped.
- Orphan interface + NM profile reconcile at startup — removed bridges / bonds get their NM profiles and sysfs interfaces cleaned up automatically.
- NM MAC fields encoded as DBus byte arrays, NM DNS fields encoded as the correct family-specific DBus type — apply paths align with what NetworkManager expects.
- Per-connection NM apply errors surface individually in the WebUI for targeted troubleshooting.
- Discovery daemons (`samba-wsdd`, `avahi-daemon`) restart on every network apply, so newly-added bridges / bonds / VLANs stay visible in macOS Finder, Windows Explorer, and Linux file managers (#270).
- Network form validates IP / CIDR before submit (#202).

### Backups

- S3 backup profile create form exposes the `region` field (#212).
- Backup profile forms expose SFTP `port` and retention `keep_yearly` (#213).

### System & updates

- `nasty-top` bumped to 0.0.5 — tuning advisor now shows `HINT` lines with reasoning instead of one-key-apply suggestions; device error counts are session-aware (pre-existing counts dim, only growth highlights bold red); `Ctrl-C` quits from any mode; device list grouped by label and natural-sorted (`sda1 < sdz1 < sdaa1`, `nvme0n1 < nvme10n1`).
- Glossary additions: **Caddy** and **Audit Log** entries on the Help page.
- `openssl` and `uv` / `uvx` now on PATH — cert inspection, TLS handshake debugging, and Python-tool one-shots work directly from the box's shell.

### Engine reliability

- **State-file handling preserves data on parse failure.** Eight state files (`auth.json`, `settings.json`, `alerts.json`, `nut.json`, `tailscale.json`, `passthrough.json`, `tuning.json`, `rate-limit.json`) now back the existing file up as `.corrupt.<unix-ts>` before falling through to defaults, log a warning, and continue — so a malformed JSON stays recoverable instead of being overwritten.
- Engine startup is robust to slow / unreachable OIDC IdPs and Caddy admin APIs — both moved to background spawns with bounded retry budgets, so the engine reaches ready state quickly regardless of upstream latency.
- Audit log coverage expanded: `permission_denied` on role-denied RPCs, `terminal_opened` / `vm_console_opened` / `log_stream_opened` on privileged WebSocket opens, and unsafe-deploy entries carry the actual admin's username.
- Six engine paths gained observable warning logs when subprocesses like `bcachefs`, `losetup`, `blkid`, or `stat` surface errors — the journal now explains what fell back to defaults instead of swallowing the cause.
- WebSocket robustness: server-initiated ping/pong + exponential client backoff (#207); reconnect overlay debounced 800ms so the UI only signals real disconnects (#205).

### CI / infrastructure

- systemd-hardened `nasty-engine` and `nasty-metrics`: NoNewPrivileges, LockPersonality, RestrictSUIDSGID, ProtectClock, RestrictRealtime, KeyringMode=private, RestrictAddressFamilies on both; full `Protect*` namespace lockdown plus ProtectSystem=strict on metrics.
- Workspace tests went from ~410 to ~440 — three previously-empty crate test harnesses (`nasty-apidoc`, `nasty-backup`, `nasty-snapshot`) gained meaningful coverage.
- Cargo / npm / rnix / rowan all bumped to current.
- HTTPS + WSS + security-header smoke assertions in the appliance-smoke CI; `/apps/<name>/` ingress short-circuits the registry pull when the image is already local.

### Bug fixes

- Compose apps' `ingress_subdomain` was silently dropped on first set (#247).
- PathPicker reverted to root on directory click (#252).
- App deploy WS close mid-`docker create_container` no longer shows a false "Connection closed unexpectedly" modal (#208).
- ACME issuance had two separate PATCHes (automation + automate) that could cancel each other mid-flight on rapid changes; collapsed into one atomic PATCH.

## v0.0.7

> **This is the NetworkManager-migration release.** v0.0.7 runs both the legacy networking layer and NetworkManager in parallel so existing installs migrate transparently. **v0.0.8 will drop the compatibility shim** — once you're on 0.0.7 and your network reconciles cleanly, you'll be ready for 0.0.8. Boxes still on 0.0.6 or earlier should not jump straight to 0.0.8.

### Headline changes

- **Networking moved to NetworkManager**, with a confirm-or-rollback safety net. Network edits stage, apply, and revert automatically if you don't confirm in time — no more SSH-locking yourself out from a typo. The WebUI surfaces risk-classified change previews, an active-edit banner with countdown, and per-connection DNS. (PRs #75–#94, #103–#110, #120, #122, #123, #127, #128)
- **Encrypted filesystem lifecycle is now end-to-end.** Lock / unlock / mount-with-keyring-key all work, the dashboard shows a "locked" alert with one-click recovery, and the WebUI warns about every app, VM, share, and backup that would break before you lock — including a per-row "🔒 on tank" badge linking to the unlock dialog. (#112, #115, #121, #124, #125, #126)
- **Hardware passthrough has a real UI.** IOMMU groups, system / BIOS / DIMM summary, USB devices, and a passthrough toggle that survives reboots. VMs can be created or edited with USB passthrough, network bridge selection, and an inline disk-import wizard. (#117–#119, #128, #133, #150–#153, #155, #165)
- **Subvolumes overview is the new default landing view.** One table grouped by filesystem, with real disk-usage progress bars (proper ceiling per subvolume type), block-image actual-allocation reporting, and a self-healing reconcile on engine startup. (#169, #174, #176, #177, #179)
- **Update flow is dramatically more reliable.** The dev-build channel now refreshes all flake inputs (kernel finally bumps), wrapper-flake templates rebootstrap on drift, failed rebuilds dump the switch-to-configuration journal so you can see what went wrong, and `nasty-cleanup` is now a one-shot fix for `/boot` full. (#157, #160–#163, #175, #180, #182, #183)

### Apps

- Inline "Enable Apps" prompt when you click Install before the Docker service is running. (#116, #129)
- Volume permission and device checks aggregate into a single warning panel instead of toast spam. (#130, #131, #149)
- Volume / backup source / ingress port pickers replaced raw text inputs with browsable paths. (#132, #134, #136, #137)
- Ingress reverse-proxy panel formatting fixed; `<name>` literal no longer renders as HTML. (#166)
- Apps view rejects bind-mount paths that don't exist on any mounted FS. (#148)
- Live per-app resource usage (CPU %, memory, network I/O, disk I/O) on the Apps page. (#185)

### Sharing

- Per-protocol panels for NFS, SMB, iSCSI, NVMe-oF — one place to see and edit each protocol's exports. (#141–#144)
- Share-creation wizard now uses the same protocol-specific forms (no more "one form fits all"). (#145)
- SMB advertises via mDNS + wsdd for Windows / macOS discovery. (#114)

### Subvolumes

- Unified overview table with filesystem group headers — alignment matches across groups. (#174)
- Size cell shows a coloured progress bar (amber 75% / red 90%) against the correct ceiling: volsize for block, quota for filesystem-with-quota, FS total otherwise. (#176)
- Block-image rows report **actual on-disk allocation** (`st_blocks * 512`) instead of the logical-sparse size, so iSCSI / NVMe-oF images no longer show as 100% full. (#179)
- **Quota inflation bug fixed:** `setquota` was passed bytes where it expected 1 KiB blocks, so every NFS PVC got a quota 1024× the requested size (a 5 Gi PVC ended up with 5 TiB). Engine now divides correctly; startup reconcile auto-rewrites existing inflated quotas. (#181)
- Project IDs back-filled at startup for subvolumes created before always-assign landed. (#177)
- Wizard's advanced bcachefs knobs collapsed behind disclosures. (#167, #168)

### Files / backups

- Files page now supports rename, in-place edit, and sortable columns. (#135)
- Backup wizard has a proper source picker. (#137)

### Updates / system

- Weekly nixpkgs-bump bot landed, with curated package-version diff in the PR body. (#147, #172)
- Dev-build channel correctly refreshes `nixpkgs` + `bcachefs-tools` + `nasty` (kernel-not-bumping bug). (#175, #180)
- Wrapper-flake content hash drives rebootstrap-on-drift; the upstream template flowing onto existing installs no longer needs manual rebootstrap. (#157, #160, #161)
- `/boot` free-space alert with `nasty-cleanup` as the one-shot remedy. (#156, #182, #183, #186)
- bcachefs-tools bumped to 1.38.3. (#154)

### CI / infrastructure

- aarch64 engine, webui, and bcachefs-tools binaries now pushed to `nasty.cachix.org` — Pi / Odroid / Rockchip boxes get cache hits instead of compiling Rust + npm locally every upgrade. (#184)
- Cachix push folded into the integration workflow (one build, not two). (#139)

### Bug fixes

- Setquota 1024× quota inflation on filesystem subvolumes. (#181)
- Block subvolume size cell stuck at 100% because `metadata.len()` returned logical-sparse size. (#179)
- Dev-build upgrade button only refreshed the `nasty` input, never `nixpkgs` or `bcachefs-tools` — explained the "kernel won't update" reports. (#180)
- `<name>` literal rendered as HTML element in Apps page. (#166)
- VM-import auto-naming included image-format suffixes (`.qcow2`, `.img`). (#164)
- WebSocket reconnect didn't refresh sysInfo, so the layout footer showed stale data. (#163)
- `/run/booted-system/kernel` vs `/run/current-system/kernel` reboot-required check (multiple update-path fixes). (#162)
- Orphan network interfaces left behind after bond/bridge deletion now cleaned up. (#120)
- Filesystem mount uses the keyring key directly instead of re-prompting. (#121)

## v0.0.6 — 2026-05-08

### Highlights

- **OIDC / SSO login support.** SSO configuration moved into Access Control. (PRs from `auth-oidc-sso` and `webui-move-sso-config`)
- **Auth hardening.** Browser session is now an httpOnly cookie, the legacy `?token=` URL fallback is gone, per-IP rate limit + persisted lockouts with an Admin-only escape hatch, and constant-time comparisons / SMB-guest / OIDC-SSRF cleanups bundled in.
- **Security hardening across the surface.** Compose deploys sandboxed, engine systemd unit hardened, NFS exports tightened, WS endpoints gated with origin validation, `{@html}` XSS sinks removed, HTTP security headers added, audit-log rotation fixed.
- **Apps `allow_unsafe` escape hatch** surfaced in the deploy/edit form and the app list (badge), for cases where the strict sandbox is too tight.
- **Test infrastructure built out** — bcachefs smoke, appliance integration smoke, JSON-RPC framing tests, alerts evaluation, sharing config, storage parser, JSON-RPC appliance smoke, pinned Rust toolchain, CI test gate. (#22–#36)
- **Big dependency bumps**: rusqlite 0.34 → 0.39, sha2, rand, x509-parser, bollard, reqwest, openidconnect 4, vitest 4. (#44, #45, #47, #48, #49)

### Other changes

- Alerts evaluated by a background notifier instead of waiting on a browser-attached client.
- Network bridge support. (#39)
- MTU configurable on connections; input crash on Apply fixed. (#63, #64)
- Encrypted filesystem no longer shows as "locked" after a successful unlock. (#59)
- ISO releases marked as pre-release by default on GitHub. (#60)
- bcachefs-tools bumped to v1.38.2.

## v0.0.5 — 2026-05-02

### Highlights

- **Backup system polished** — friendlier create wizard, human-readable schedule + next-run on cards, Edit button on profiles, config-backup warning banner with one-click "Create backup" shortcuts, dismiss control. Daily ACME cert renewal check, configurable DNS-propagation timeout, TLS cert details parsed in Rust (no `openssl` shell-out).
- **Services page unified.** SSH config, UPS config, Docker enable/disable, Backup-server storage path, and per-service Configure panels all live in one place now.
- **Access Control rebuilt.** System users and groups shown together, click a user to manage group memberships, inline user creation in the share wizard, last-admin can no longer be deleted, share wizard uses a real user/group picker.
- **Installer fixes** — explicit `mount -t` for partitions, partprobe + udevadm settle + sync after format, ext4 reserved blocks at 1%, installer text matches actual partition size, TTY banner skips link-local addresses.
- **Sidebar search bar** for quick navigation.
- Filesystem label now equals the user-chosen name on `bcachefs format`.

### Cleanups

- Removed all backward-compatibility hacks accumulated through 0.0.x.
- Removed GitHub token auth path now that repos are public.
- Dashboard SMART section retired (already visible in Disks).

## v0.0.4 — 2026-04-21

### Highlights

- **Apps runtime replaced**: k3s + Helm → Docker + bollard. Much smaller closure, faster install, no k8s overhead for a single-node appliance.
- **Live deploy streaming** for app installs, compose deploys, and `docker pull`. Per-container Shell and Logs for compose apps.
- **Apps lifecycle**: stop/start, restart, pull, prune, container details, ports, compose ingress, port-conflict detection with auto-suggest, default host port = container port, auto-detect EXPOSE.
- **Compose YAML editor** (CodeMirror) with error-line marking.
- **File preview + download** in the Files browser.
- **bcachefs-tools 1.38.0** + nixpkgs bump.
- Per-subvolume bcachefs options exposed in the WebUI.
- BIOS warning during install when booted in legacy mode (must reinstall in UEFI).

### Fixes

- Filesystem destroy now wipes superblocks reliably; stale signatures no longer block re-use of devices.
- Mount/unmount and other long operations now give live feedback.
- `nasty-top` integrated into the appliance PATH.

## v0.0.3 — 2026-04-13

### Highlights

- **Tailscale VPN integration** — enabled by default on all NASty appliances, simple Connect / Disconnect UI.
- **NUT (Network UPS Tools)** support for local UPS monitoring, configured from Settings.
- **Apps** got auto-assigned NodePorts and nginx ingress, `/apps/{name}/` proxy links replacing port-forward, auto-detected EXPOSE ports, in-place editing via `helm upgrade`.
- **NAS tuning settings** (NFS threads, SMB, iSCSI, VM writeback) exposed in the UI.
- **Filesystem options**: `journal_flush_delay`, `io_scheduler`, `fs.reconcile.enable/disable`, checksum options in the edit panel, erasure-coding indicator (gated on disk count).
- **Audit log** records all mutations; new `audit.list` API.
- **Kernel error monitoring** with alert rules.
- WebUI Licenses page; GPL-3.0 LICENSE file + third-party inventory added.

## v0.0.2 — 2026-04-06

### Highlights

- **Flake-based system architecture.** Slim installed wrapper at `/etc/nixos`, upstream pulled in via flake inputs — system upgrades stop being a `git pull` and become a `nix flake update`.
- **Offline-capable ISO installer.** Bootstraps without network access.
- **`nasty.cachix.org` binary cache** added — fast appliance updates instead of building Rust + npm locally.
- **Disk Topology tab** with controller / port mapping, plus ATA port mapping in disk health.
- **Periodic auth check** detects expired sessions and bounces the user to login instead of leaving stale UI.
- **Performance**: merged xattr reads into a single `list+get` pass per subvolume, batched `du` / `losetup` queries.
- `croc` added to the appliance for debug-report transfers.
- INSTALL.md added with an alternative install-from-Linux-live-environment recipe.

## v0.0.1 — 2026-04-01

Initial public release. NixOS-based NAS appliance built on bcachefs.

### Foundations

- bcachefs storage with project-quota-aware subvolumes (nested allowed, `.nasty/*` for internals).
- WebUI with Apps, VMs, Subvolumes, Sharing, Backups, Files, Network, Update, and Help pages.
- Three release flavors: **Mild** (`v*` tags, stable), **Spicy** (`s*` tags, snapshots), **Nasty** (`main` branch, dev builds) — all picked from a single flake.
- Engine `--version` flag, in-WebUI engine version detection with auto-reload on change.
- ISO build workflow (GRUB EFI + systemd-boot variant for picky UEFI firmware).
- Periodic config backup from `/var/lib/nasty` to bcachefs.
- Backup system using rustic (deduplicating, encrypted).
- Quota / size support for filesystem subvolumes.
- Help menu with community links.
