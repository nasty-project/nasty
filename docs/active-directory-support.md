# Active Directory Support

Design for NASty's Active Directory integration, in two phases:

1. **Member join** (this document's main scope) — NASty joins an existing AD
   domain; domain users and groups become usable in SMB share permissions.
2. **Domain Controller role** (appendix) — NASty hosts the domain itself,
   replacing a Synology Directory Server / Windows Server DC.

Requested in [issue #20](https://github.com/nasty-project/nasty/issues/20).
The reporter runs a Synology as their only DC, so full replacement needs
phase 2 — but phase 1 is a hard prerequisite: the box's own file services
consume domain identities the same way whether the domain lives on-box or
elsewhere. This matches the industry split: TrueNAS ships member join only;
Synology ships member join in DSM core and the DC as a separate opt-in
package.

## Phase 1: AD member join

### Scope

- Join and leave an existing AD domain, per box (operator opt-in).
- Domain users and groups usable in SMB share `valid_users`, resolved live
  through winbind — never copied into the engine's user registry.
- SMB only. Out of scope for this phase: domain accounts logging into the
  WebUI/API or SSH, and kerberized NFS. Both can be added later without
  redesign.

### Components

- **`nasty-system`: new `domain` module.** Owns join state, krb5 and winbind
  configuration, preflight checks, and health. Lives in `nasty-system`
  because it is system identity (krb5.conf, winbindd, NSS), not sharing;
  `nasty-sharing::smb` and the WebUI consume it.
- **Engine methods** (registry naming follows `smb.user.list` precedent):
  - `domain.join` — realm, AD admin credential, optional OU, optional idmap
    base. Credential is used once for `net ads join` (passed via stdin,
    never argv, never persisted).
  - `domain.leave` — with credential, or forced local-only leave.
  - `domain.status` — joined state, trust health, DC reachability, clock skew.
  - `domain.user.list` / `domain.group.list` — require a search prefix;
    query winbind on demand. No wholesale enumeration (large domains make
    `wbinfo -u` explode); the WebUI picker is search-driven.
- **NixOS module (`nasty.nix`):**
  - Samba package built with `enableLDAP = true` (nixpkgs defaults to
    `--without-ldap --without-ads`; ADS member mode needs both). See Risks.
  - winbindd shipped and defined like smbd/nmbd: `wantedBy = []`, started by
    the engine only when joined.
  - winbind NSS module in `system.nssModules` permanently; inert (instant
    not-found) while winbindd is not running.
  - krb5 configuration rendered by the engine at join time via the existing
    include-file pattern — join is a runtime operation, not a rebuild.
- **State:** `DomainConfig` JSON in the engine state dir (realm, workgroup,
  idmap base, join status). No secrets — the machine account secret lives in
  Samba's `secrets.tdb` under `/var/lib/samba`.

### Join flow

1. **Preflight** — fail with clear errors before Kerberos produces cryptic
   ones: resolve `_ldap._tcp.<realm>` SRV, check DC reachability on the
   required ports, check clock skew against the DC (Kerberos tolerates
   ~5 minutes).
2. Render krb5 and smb.conf ADS config; start winbindd.
3. `net ads join` with the admin credential on stdin.
4. Verify the trust with `wbinfo -t`.
5. Register the box's A record in AD DNS (`net ads dns register`).
6. Add a per-domain DNS routing rule (systemd-resolved) so the box resolves
   the AD zone through the DC's DNS without replacing its resolvers.
7. Persist `DomainConfig`; mark joined.

Any failure rolls configuration back to the pre-join state (same spirit as
the network transaction machinery). Leave reverses the flow and documents —
rather than cleans up — files owned by domain UIDs and share ACLs that
reference domain principals; they become orphans, consistent with how
TrueNAS and Synology behave.

### Identity model

- The engine registry stays authoritative for **local users only**. Domain
  principals are a live external view via winbind; they are not editable in
  NASty (passwords and group membership are AD's job).
- **UID mapping:** `idmap_rid` — algorithmic, stateless, stable across
  reboots, rejoins, and reinstalls. Default base 100000 (well above the
  local `SMB_USER_UID_MIN = 3000` range), configurable at join,
  **immutable afterwards** — changing it would re-map ownership of every
  domain-owned file. The engine enforces immutability and collision-checks
  the range at join.
- Domain principals appear as `DOMAIN\name` everywhere. We deliberately do
  not set `winbind use default domain` — the prefix keeps domain and local
  namespaces unambiguous.
- `valid_users` validation gains a deliberate carve-out for the backslash in
  `DOMAIN\name` entries, with the same injection-safety test treatment
  `validate_share_path` has (newline, `#`, quote smuggling).

### Config rendering

When joined, the engine-rendered global include gains the ADS block:
`security = ADS`, realm, workgroup, idmap ranges, and
`winbind offline logon` so cached domain users survive a DC outage.
Unjoined boxes render byte-identical config to today.

Local passdb authentication continues to work alongside ADS mode.
**Degradation contract:** DC unreachable ⇒ domain-account auth degrades
(mitigated by winbind caching for recently seen users); local users and
shares are unaffected.

### Health

`domain.status` is wired into the existing monitoring surface: trust secret
check (`wbinfo -t`), DC reachability, clock skew. This is also the safety
net for the machine-account-rotation risk below — a broken trust is
surfaced with a re-join action instead of operators discovering shares
failing.

### Testing

- Unit tests: config rendering, `DOMAIN\name` validation including
  injection cases, idmap range collision checks.
- NixOS VM test (two nodes): a throwaway provisioned Samba DC and an
  appliance node that joins it, authenticates a domain user via smbclient,
  writes a file, and asserts ownership lands in the idmap range. Gates the
  Samba package rebuild as well as the feature.
- Engine-level integration tests can use the `smblds/smblds` container
  (a Samba AD DC image built for CI).

### Risks

- **Samba rebuild affects every box.** `enableLDAP = true` changes the Samba
  binary all boxes run, joined or not: same upstream source, more features
  compiled in, built from source instead of the binary cache, and new build
  inputs (OpenLDAP and friends) that must be wired into the Nix derivation.
  Mitigation: the existing SMB coverage in the appliance VM test runs
  against the new build and gates the swap; only the Integration workflow
  (sandboxed build) proves the derivation inputs are complete.
- **Machine account password rotation vs rollback.** AD machine accounts
  rotate their password (30-day default) into `secrets.tdb`. A version
  rollback or restored snapshot that resurrects an old `secrets.tdb`
  silently desyncs the box from the domain. **Resolved:** `/var/lib` is
  state on the root filesystem, untouched by `nixos-rebuild` generation
  switches or rollbacks (`nixos/modules/nasty.nix:1128`). The root
  filesystem itself is always plain ext4 on its own partition, created by
  the installer (`nixos/iso.nix:248,254,268`) and never registered with
  `nasty-storage`'s `FilesystemService` — every NASty-managed bcachefs pool
  mounts under `/fs/<name>` (`NASTY_MOUNT_BASE`,
  `engine/nasty-storage/src/filesystem.rs:14`), and both subvolume rollback
  (`engine/nasty-engine/src/subvol_rollback.rs`) and snapshots
  (`engine/nasty-snapshot`) operate exclusively through that
  `SubvolumeService` registry. So no rollback or snapshot-restore path can
  reach `/var/lib/samba/private/secrets.tdb` at all — rotation stays on,
  and no config change is needed here. `domain.status` remains the safety
  net for the (now purely theoretical) broken-trust case.
- **Time dependency.** Kerberos needs sane clocks. Preflight checks skew at
  join; `domain.status` monitors it afterwards.

### Footprint on boxes that never join

winbindd installed but never started; NSS module present but inert;
rendered Samba config byte-identical; no firewall changes (member mode
needs outbound only); no new state. The one shared change is the rebuilt
Samba binary (see Risks).

## Phase 2 appendix: Domain Controller role (decisions to date)

Not designed in detail yet — these are the decisions settled during
brainstorming, recorded so the phase-2 spec starts from them.

- **Shape: native, in a NixOS (systemd-nspawn) container** — not a Docker
  app, not on the host directly. The AD DC process runs its own embedded
  smbd (SYSVOL) and needs ports 53/88/389/445 on its own address, which
  collides with the host's file-serving smbd; a declarative
  `containers.<name>` gives it its own network namespace and state
  directory with no third-party image dependency. Samba upstream
  discourages mixing the DC role with general file serving; this keeps them
  separated on one box, the same way Synology's Directory Server package
  is separated from DSM's file services.
- **Kerberos:** nixpkgs' Samba builds the AD DC with bundled **Heimdal**
  when `enableDomainController = true` — the upstream-supported KDC
  configuration (MIT KDC remains experimental). The DC-capable build lives
  only in the container's closure; the host Samba binary is unaffected.
- **Networking: macvlan child + host-side macvlan shim**, both
  engine-managed, parent interface chosen per box at role enable. The shim
  exists because a host cannot reach its own macvlan children, and the
  host's smbd must reach the DC's DNS/Kerberos/LDAP to join the hosted
  domain. Bridging the uplink was rejected as too invasive to existing
  network config; routed/NAT was rejected because AD behind NAT breaks.
- **DC address:** static, user-chosen at provisioning (realm, NetBIOS
  domain, admin password, DC IP) — clients bootstrap DNS from it, so it
  must not drift.
- **NASty's own file services join the hosted domain as a member** — which
  is why phase 1 is a prerequisite.
- **Opt-in:** per-box role; zero footprint (no container, interfaces,
  ports, or state) until enabled. Whether the container closure ships in
  the base image (size cost) or is fetched on enable is a phase-2 decision.
- **Open questions for phase 2:** DNS handoff details (how the box itself
  and DHCP clients get pointed at the DC's DNS), backup/restore lifecycle
  (`samba-tool domain backup` wired into the backup story), Samba version
  upgrades of the domain database (`samba-tool dbcheck`), and whether
  multi-DC (a second NASty as replica) is ever in scope.
