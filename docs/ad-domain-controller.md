# Active Directory: NASty as the Domain Controller

Design for hosting an AD domain on a NASty box — the second half of
[issue #20](https://github.com/nasty-project/nasty/issues/20). The first
half, member mode (NASty joins an *existing* domain), shipped in #627 and is
documented in `docs/active-directory-support.md`. This feature is the
inverse: NASty *is* the domain, replacing a Synology Directory Server or a
Windows Server DC. The #20 reporter runs a Synology as his only DC today —
that is the setup this replaces.

Strategic note: TrueNAS removed its DC role years ago; Synology still ships
Directory Server as a package. A NAS that can host the domain — and whose
sibling boxes can join it with the member mode we already ship — is a real
differentiator.

## Scope

**In scope (v1):**
- Provision a **new** AD domain on this box (`samba-tool domain provision`,
  Samba AD DC, SAMBA_INTERNAL DNS) from the WebUI: realm + Administrator
  password + optional DNS forwarder.
- **Exactly one DC per domain.** v1 never joins an existing domain as an
  additional DC.
- The DC **serves the box's file shares** through its own integrated smbd —
  the existing `smb.nasty.conf` include chain rides along unchanged, and
  share ACLs reference the AD users/groups the box hosts.
- Domain user/group/computer management in the WebUI (the Synology-DS-sized
  surface): users (list/create/delete/set password/enable/disable), groups
  (list/create/delete/membership), computers (read-only list).
- **Domain backup** as a first-class operation: `samba-tool domain backup
  offline` into an operator-chosen path jailed under `/fs`, where the
  existing backup profiles (rustic) can ship it offsite and #635's restore
  can bring it back.
- Demote (= destroy the domain, in a single-DC world) with heavy
  confirmation and an automatic final backup.
- Mutual exclusivity with member mode, enforced both ways.

**Out of scope (explicitly deferred):**
- Joining an existing domain as an **additional DC** (DRS replication, FSMO
  management, SysVol replication) — the real HA answer, and its own
  brainstorm when it comes.
- BIND9_DLZ DNS backend (SAMBA_INTERNAL only).
- GPO / OU / password-policy / delegation / trust management in the WebUI.
  The sysvol and GPO infrastructure exists on the DC automatically; Windows
  RSAT manages it natively — we do not rebuild RSAT.
- Signed NTP for domain clients (documented limitation).
- Automated domain **restore** in the WebUI. v1 documents the manual
  procedure (fresh box, `samba-tool domain backup restore`, runnable from
  the WebUI terminal); automating it is a named follow-up, not a silent gap.

## Fleet story (costs nothing, worth stating)

Member mode is already validated against a Samba AD DC — the CI member test
joins one. So the moment this ships, one NASty hosts the domain and every
other NASty joins it with existing, shipped code. A NASty-only fleet with
centralized identity, no Windows Server anywhere. The v1 VM test proves
exactly this pairing.

## Architecture

A new `engine/nasty-system/src/dc.rs` (`DcService`), sibling to member
mode's `domain.rs` — Approach B from the brainstorm. `domain.rs` (freshly
validated against a real DC) is not restructured; it only gains the inverse
mutual-exclusion check. Shared logic (`validate_realm`, `derive_workgroup`,
the resolved-drop-in pattern) is imported from `domain.rs`, which already
exports it — not duplicated.

### Packaging: one samba build

nasty.nix's `sambaAds` (`enableLDAP = true`) becomes the DC-capable superset
(`enableLDAP = true; enableDomainController = true;` plus the
`python3Packages.cryptography` pythonPath addition the CI DC build already
carries for samba-tool's provision path — check whether the pinned nixpkgs
still needs the backport at implementation time). One build serves both
roles; member boxes simply never run the DC bits. Two parallel samba store
paths would invite version skew. `samba-tool` joins the engine service's
PATH. This changes the samba closure on every box — the nix-sandbox /
Integration workflow is the gate that catches packaging fallout, not local
cargo builds.

### Provision flow (`dc.provision { realm, admin_password, dns_forwarder? }`)

1. **Preconditions, all checked up front with pointed errors:**
   - not already hosting a domain;
   - not joined to a domain as a member (probes member mode's persisted
     config; `domain.join` gains the mirror check against `dc.json`);
   - **static IP** — the interface carrying the box's primary address (the
     one clients reach the DC on) must be statically configured in NASty's
     network config; a DHCP-addressed DC is a time bomb. The error names
     the Network page as the fix;
   - port 53 must be freeable (resolved's stub listener is ours to move;
     anything else squatting on :53 is an error).
2. **Provision:** `samba-tool domain provision --realm=<REALM>
   --domain=<WORKGROUP> --server-role=dc --dns-backend=SAMBA_INTERNAL`
   against an **engine-owned config path** `/etc/samba/smb.dc.conf`
   (samba-tool's `--configfile`). The nix-managed `/etc/samba/smb.conf` is
   never touched — on NixOS it is a store symlink and must stay one. The
   workgroup is derived from the realm (`derive_workgroup`).
3. **Shares include:** after provision, the engine appends the existing
   NASty include chain (`include = /etc/samba/smb.nasty.conf`) plus
   NASty-required globals to `smb.dc.conf`, so the DC's integrated smbd
   serves sysvol/netlogon *and* the box's shares — decision (a) from the
   brainstorm, the pragmatic single-box pattern. Samba upstream's
   "don't file-serve from a DC" caveat is documented, not hidden. Note:
   the DC allocates user/group IDs from its own `idmap.ldb`, not member
   mode's `idmap_rid` math — on-disk UIDs differ between a DC-mode box and
   a member-mode box. Fine standalone; documented.
4. **Credential hygiene (same rule as member join — secrets never ride
   argv):** `--adminpass` on argv is visible in `/proc` for the life of the
   process. Provision therefore runs with a **random throwaway password on
   argv**, and the operator's real Administrator password is set immediately
   after via `samba-tool user setpassword Administrator` **fed over stdin**,
   then never logged and never persisted.
5. **DNS:** SAMBA_INTERNAL owns :53. A `/run/systemd/resolved.conf.d/`
   drop-in (same mechanism member mode uses) sets `DNSStubListener=no` and
   points the box's own resolution at `127.0.0.1` (the samba DNS).
   `dns forwarder = <upstream>` in `smb.dc.conf` keeps external resolution
   working — operator-suppliable, defaulting to the box's current upstream
   resolvers. Clients then use the NASty DC as their DNS for the AD zone.
6. **Service switchover — systemd `Conflicts=`:** a new `samba-dc.service`
   in nasty.nix (present on every box, disabled by default, engine-toggled —
   per-box opt-in as always) runs `samba --foreground
   --configfile=/etc/samba/smb.dc.conf` and declares `Conflicts=` +
   `After=` against `samba-smbd`/`samba-nmbd`/`samba-winbindd`, so starting
   the DC atomically stops the member-mode daemons and stopping it lets
   them return. The engine records the role in `/var/lib/nasty/dc.json` and
   its boot-restore phase re-establishes it after reboot.
7. **Firewall:** a `dc` service-rule set in the firewall module (same shape
   as `rdma_ports()`): tcp+udp 53, 88, 464; tcp 135, 139, 389, 445, 636,
   3268, 3269; udp 137, 138; plus the dynamic RPC range tcp 49152–65535.
   Opened when the role activates, closed on demote. The RPC range is the
   firewall's first ranged *service* rule: `PortSpec` gains an optional
   range end (`to: Option<u16>`, serde-default, additive — existing
   persisted state unaffected) and the service render loop emits
   `dport from-to` when set, mirroring what #637's custom rules already
   render. No NTP port: the box does not serve time (see 8).
8. **Time:** the box keeps timesyncd as an NTP *client*; serving signed NTP
   to domain clients is out of scope and documented.

### State & disaster recovery

- **Domain state stays on the root ext4** at samba's default
  `/var/lib/samba` — no relocation to `/fs`. The DC is auth infrastructure;
  it must come up even when the pool has trouble, and bcachefs snapshots of
  *live* ldb databases aren't transactionally consistent anyway.
- **`dc.backup { dest }`** runs `samba-tool domain backup offline`
  (the transaction-safe copy of the domain databases) with a target dir
  **jailed under `/fs`** — same canonicalize-and-check validation shape as
  restore (#635). The resulting tarball is ordinary `/fs` data: a rustic
  backup profile ships it offsite; #635's restore brings it back to a new
  box; `samba-tool domain backup restore` (documented, WebUI terminal)
  resurrects the domain. The DR story composes entirely from parts that
  already shipped.
- **Demote takes a parachute:** before tearing anything down, a final
  domain backup is written into `/fs` (when a filesystem exists; if none
  does, the confirmation says so in red).

### Demote (`dc.demote { realm_confirmation }`)

Single-DC world: demote **destroys the domain** — every user, group, and
joined machine's trust. Accordingly: the request carries the typed realm
(exact match), the WebUI frames it as a danger-zone action, and the engine
sequence is: final backup → stop `samba-dc` → remove `smb.dc.conf` + domain
state → drop the resolved drop-in → close the `dc` firewall rules → delete
`dc.json`. The box returns to standalone; SMB units come back under their
normal protocol toggles. A provision that fails mid-flight unwinds through
the same teardown path (minus the backup), surfacing the failing step — the
member-join work proved unwind discipline has to be there from day one.

## Engine surface

`DcService` methods (all `samba-tool` invocations use the engine-owned
config via `--configfile`; **every password travels over stdin, never
argv**):

- `provision(ProvisionRequest) -> DcStatus`
- `demote(DemoteRequest) -> ()`
- `status() -> DcStatus { hosting, realm, workgroup, dns_forwarder,
  service_healthy }` — `service_healthy` = `samba-dc.service` active;
  cheap enough for the page to poll.
- Users: `user_list`, `user_create { name, password, given_name?,
  surname? }`, `user_delete`, `user_set_password`, `user_enable`,
  `user_disable`.
- Groups: `group_list`, `group_create`, `group_delete`,
  `group_add_member`, `group_remove_member`.
- Computers: `computer_list` (read-only).
- `backup { dest } -> { path }` — dest jailed under `/fs`, created if
  missing, must be empty (samba-tool requires an empty target dir).

Every samba-tool failure returns stderr verbatim in the error — the
member-join experience showed real deployments fail in ways only the real
message explains.

## RPC / registry

`dc.*` namespace in a new `engine/nasty-engine/src/router/dc.rs`:
`dc.status` (role `Any`); **everything else `Admin`** — hosting identity
infrastructure is above Operator's pay grade, matching
`system.firewall.restrict`'s posture. Admin methods are enforced by
omission from both allowlists (the #635 lesson: the registry role is
declarative; the allowlist is the gate — Admin-only methods are correct by
construction, and the `operator_role_methods_are_operator_allowed` guard
test doesn't apply).

## WebUI

The Domain page becomes three-state:

- **Standalone:** two cards — "Join an existing domain" (existing member
  flow) and "Host a new domain," with an honest blurb: this NASty becomes
  the domain controller; one DC per domain; back it up; clients should use
  it as DNS.
- **Member:** the existing member UI, untouched.
- **DC:** a dashboard — realm, workgroup, service health; Users / Groups /
  Computers tabs with inline create / reset-password / enable-disable /
  membership actions; a "Back up domain" action with the `/fs` destination
  picker; Demote in a danger zone requiring the typed realm. Copy notes
  RSAT works against this DC for advanced administration (OUs, GPOs,
  policies).

Password fields follow the existing conventions (never echoed back;
`domain.join`'s credential handling is the template).

## Error handling

- Precondition failures → `InvalidParams`-style errors before anything
  changes, each naming the fix (static IP → Network page; joined → leave
  first; already hosting → demote first).
- Mid-provision failure → unwind (teardown path), error names the step.
- Provisioned but `samba-dc` won't start → `status.service_healthy = false`;
  the UI shows unhealthy with a journal pointer
  (`journalctl -u samba-dc`).
- Backup target not under `/fs` / not empty → refused up front, same jail
  errors as #635.
- Demote realm mismatch → refused, nothing touched.

## Testing

- **Unit** (`dc.rs`): precondition matrix (joined ↔ hosting exclusion,
  static-IP check logic); config-render fragments (include-chain append,
  dns forwarder line); the **argv-hygiene invariant** — no constructed
  `samba-tool` command line ever contains a secret (directly testable);
  backup-dest jail (reuse the #635 test shapes); demote confirmation
  matching.
- **VM test — the money shot** (`nixos/tests/ad-dc.nix`): NASty box A
  provisions a domain via the RPC path the WebUI uses; creates a user;
  NASty box B **joins that domain with the shipped member flow**, resolves
  the user through winbind, authenticates over SMB against B, writes a file
  owned by the domain user. One test proves the whole fleet story —
  NASty DC + NASty member, no Windows anywhere. Harness cribbed from
  `ad-member.nix` (whose throwaway DC this feature effectively
  productizes). Wired into the Integration workflow's path filter.
- **Real-world:** a Windows client joined to the hosted domain + RSAT
  against it (lab); johnnyq as the Synology-Directory-Server-replacement
  validator — the acceptance test we can't fully synthesize.

## Follow-ups (explicitly not v1)

- Automated domain restore in the WebUI (v1 documents the manual
  `samba-tool domain backup restore` procedure).
- Additional-DC join / replication / FSMO (the HA story).
- Scheduled domain backups (v1: manual button; trivial once the scheduler
  grows a hook).
- Signed NTP for domain clients.
