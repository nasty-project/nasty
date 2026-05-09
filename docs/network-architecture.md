# Network Architecture (proposed)

> Status: design proposal. Not implemented. Read alongside the current
> `engine/nasty-system/src/network.rs` to see what exists today.

This document proposes a long-term direction for how NASty manages the host
network. It's motivated by issue #74 (bridge over the management iface
dropped connectivity) and the six follow-up PRs (#75, #76, #77, #80, #85,
#90), which fixed the immediate symptoms but left the underlying model
unchanged. The goal here is to choose an architecture that handles the next
class of problems — Docker, VMs, multiple egress interfaces, per-service
binding — without a similar firefighting series each time.

## What's wrong with today's model

The current network code in `nasty-system/src/network.rs` has three
properties that won't scale:

1. **Dual-layer apply.** Runtime changes go through `ip` commands;
   persistence goes through a generated `/etc/nixos/networking.nix` plus a
   follow-up `nixos-rebuild boot` (added in #90). The two layers can drift.
   Most of #74's follow-ups are about narrowing that drift; none of them
   eliminate it. And every change pays a 30-second `nixos-rebuild` for
   what should be a sub-second operation.

2. **Flat data model with conflated concerns.** Each link type
   (interface, bond, bridge, VLAN) is its own struct with its own embedded
   `IpConfig`. The L2 fact ("br0 has eth0 as a member") and the L3 fact
   ("br0 has address 10.10.10.52/24") are stored together. A bridge that's
   intentionally L2-only (host-internal for VMs) has to express that as
   `IpMethod::Disabled` on the same struct, which doesn't read like the
   thing it is. Adding routing tables, IP rules, or per-service binding
   later means more fields on the same conflated structs.

3. **No ownership boundary.** The apply pipeline assumes every link in
   the kernel is either fair game to modify or doesn't exist. In reality
   the kernel namespace already has multiple writers:
   - **Docker** creates `docker0` plus a `br-<hash>` per user-defined
     network, plus veth pairs and NAT iptables rules.
   - **libvirt / QEMU** create taps attached to a bridge whenever a VM
     starts, sometimes `virbr0`.
   - **WireGuard / Tailscale** create their own tunnel ifaces.
   - **Future**: Kubernetes (`cni*`), bcachefs replication tunnels,
     VPN-per-app routing.

   Today these survive only because the apply pipeline never tries to
   *remove* anything. The first time we add removal — which the
   comprehensive plan calls for — we stand to tear down a Docker bridge or
   a libvirt tap unless the model knows not to.

The combination is the source of the bug class: NASty thinks the network
is its own; the network in fact has multiple owners; persistence and
runtime are two layers that have to be kept in sync by hand.

## Design principles

**One change → one apply path.** Runtime and reboot must execute the
*same* code over the *same* state. No "runtime is via ip, persistence is
via Nix module." That's the single biggest source of drift and the reason
every fix in #74's follow-up chain has had a "...but on reboot" caveat.

**Explicit ownership boundary.** Every link in the kernel falls into one
of three buckets:
- **NASty-owned** — appears in our state. We create, modify, can destroy.
- **External** — created by another service (Docker, libvirt, wg, ts).
  We *observe* (UI lists it, classify_risk knows it exists) but apply
  *never* touches it.
- **Foreign physical** — a NIC on the box that NASty hasn't claimed yet.
  UI presents it as "available," apply leaves it alone until claimed.

**Layered model, validated as a graph.** Separate the L2 topology (links
and how they relate) from L3 (addresses and routes) and L4 (firewall and
per-service binding). Validate desired state as a DAG before apply.
Reject cycles, missing references, double-enslavement at the API
boundary instead of letting them turn into runtime failures.

## Proposed data model

Today's model:

```rust
struct InterfaceConfig { name, enabled, ipv4, ipv6, mtu }
struct BondConfig      { name, members, mode, ipv4, ipv6, mtu }
struct BridgeConfig    { name, members, ipv4, ipv6, mtu, stp, forward_delay_s }
struct VlanConfig      { parent, vlan_id, ipv4, ipv6, mtu }
struct NetworkConfig   { interfaces, dns, bonds, vlans, bridges }
```

Proposed model:

```rust
// L2: every link is a Link, distinguished by Kind. The kind controls
// what fields are meaningful — Bridge has members + STP knobs, Vlan
// has parent + id, Physical has nothing extra. No L3 fields here.
struct Link {
    name: String,
    enabled: bool,
    mtu: Option<u16>,
    mac: Option<String>,             // explicit override; None = inherited
    kind: LinkKind,
}

enum LinkKind {
    Physical,                        // discovered, never created
    Bond { members: Vec<String>, mode: BondMode },
    Bridge { members: Vec<String>, stp: bool, forward_delay_s: Option<u8> },
    Vlan { parent: String, id: u16 },
}

// L3: addresses attach to links by name. Multiple addresses per link
// is the natural shape (dual-stack, multiple v4 addrs, link-local).
struct Address {
    link: String,
    family: Family,                  // V4 or V6
    method: AddressMethod,           // Dhcp | Static | Slaac | Inherit
    cidr: Vec<String>,               // empty for Dhcp/Slaac/Inherit
    gateway: Option<String>,
}

// L3 routing — separate from addresses. Lets you express
// "service A egresses via br0, service B via wg0" via routing rules.
struct Route { table: u32, dst: Cidr, via: Option<IpAddr>, dev: String, metric: Option<u32> }
struct Rule  { selector: RuleSelector, table: u32 }   // policy routing

// L4: firewall + per-service binding (eventually).
struct FirewallRule { ... }    // already exists in nasty-system
struct ServiceBinding { service: String, dev: String }  // future

struct NetworkConfig {
    links: Vec<Link>,
    addresses: Vec<Address>,
    routes: Vec<Route>,        // optional; default route comes from gateway
    rules: Vec<Rule>,
    dns: Vec<String>,
}
```

Why this shape:

- A bridge for VMs is `Link { Bridge { members: [] } }` with no
  `Address` — exactly what it is, no special-casing.
- A bridge with the host's IP is the same Link plus an Address pointing
  at it. Adding a second address is one more entry in `addresses`, not
  a new field.
- Per-service binding (the "service A on iface 1, service B on iface 2"
  case) is a `Rule + Route` pair that doesn't touch the L2 model at all.
- Validation has a clear shape: `Bond.members ⊂ Physical`,
  `Bridge.members ⊂ Physical ∪ Bond ∪ Vlan`,
  `Vlan.parent ∈ Physical ∪ Bond ∪ Bridge`, `Address.link ∈ links`.
  Reject cycles and dangling refs at the API boundary.

The model is **backend-agnostic** — these types describe the desired
state, not how it's realized. The backend choice below is a separate
decision.

## Backend choice: NetworkManager

Three options were considered:

| | systemd-networkd | NetworkManager | Roll our own |
|---|---|---|---|
| Apply mechanism | Write `.network` files, `networkctl reload` | DBus API: CRUD "connections," activate per-link | `ip` commands + own oneshot at boot |
| Native fit for our usage | "Static config, doesn't change much" | "Varying profiles per device" | Whatever we make of it |
| Coexistence with Docker / libvirt / wg / ts | Implicit (only manages what we configure) | Explicit `unmanaged-devices` glob | We'd reimplement |
| Programmatic API | File-based + `networkctl` shell-out | DBus (zbus crate from Rust) | None — we are the API |
| Reboot persistence | Files in `/etc/systemd/network/` | Keyfiles in `/etc/NetworkManager/system-connections/` | We'd reimplement |
| DHCP / SLAAC / IPv6 RA | Built-in | Built-in | Reimplement or shell to dhcpcd |
| Maturity on NixOS | First-class (`networking.useNetworkd`) | First-class (`networking.networkmanager.enable`) | N/A |

The stronger choice for NASty is **NetworkManager**, primarily because
NASty's usage pattern doesn't match networkd's design center.

The Arch/NixOS wiki framing of when to use networkd is explicit:

> Use systemd-networkd for setups that rely on static configuration,
> that doesn't change much during its lifetime, that does not require
> varying profiles for a single interface.

NASty's whole shape is the opposite: configuration *does* change via the
WebUI, and a single iface naturally takes on multiple profiles over its
lifetime ("eth0 is plain DHCP today; tomorrow it's a bridge member;
next month it's a bond member"). NetworkManager's "connection" abstraction
matches that directly — each profile is a connection, switching role is
activating a different connection on the same device.

NetworkManager also has two specific advantages that map cleanly onto our
existing concerns:

- **DBus API as the integration point.** We talk to NM from Rust via
  `zbus`. Connections are CRUD'd as DBus objects; activation is a method
  call; live state changes come back as property-change signals. That's
  a *real* programmatic interface, not "write file + shell out + parse."
  We can subscribe to live changes for the WebUI's network status panel
  without polling.
- **`unmanaged-devices` is the ownership boundary.** We tell NM via
  `/etc/NetworkManager/conf.d/10-nasty.conf`:
  ```
  [keyfile]
  unmanaged-devices=interface-name:docker*;interface-name:br-*;interface-name:veth*;interface-name:vnet*;interface-name:tailscale*;interface-name:wg*;interface-name:cni*
  ```
  NM will then refuse to touch those names, no matter what we do via
  DBus. The kernel still creates them (Docker/libvirt/wg/ts continue to
  work), but our backend can't accidentally fight them. This is the
  ownership boundary made structural rather than convention-based.

The tradeoffs of picking NM over networkd:

- **Heavier daemon.** ~30MB resident vs networkd's near-zero. Negligible
  on the kind of hardware NASty runs on.
- **DBus dependency.** Already present on any systemd-based system; not
  a new install footprint.
- **Slightly more API surface to learn for contributors.** Mitigated by
  NM's better documentation and the fact that the DBus API is stable and
  well-traveled.

What NM also gets us for free that we'd otherwise build:
- Connection profile diffing — NM's own `nmcli connection modify` shows
  diffs, which the WebUI can mirror.
- Device state machine (disconnected → preparing → ip-config → activated
  → deactivating). Surfaceable in the UI.
- DNS handling that integrates with `systemd-resolved` or `resolvconf`.
  Our current `apply_dns` shell-out becomes redundant.
- Connectivity check (`NetworkManager.conf` `connectivity-check-uri`).
  Natural source for the WebUI's "internet reachable" indicator.

The NASty NixOS module's networking responsibilities shrink to:

```nix
networking.networkmanager.enable = true;
networking.networkmanager.settings.keyfile.unmanaged-devices =
  "interface-name:docker*;interface-name:br-*;interface-name:veth*;...";
networking.useDHCP = false;
networking.bridges = lib.mkForce {};
networking.bonds = lib.mkForce {};
networking.vlans = lib.mkForce {};
networking.interfaces = lib.mkForce {};
```

Plus a tmpfile rule to ensure `/etc/NetworkManager/system-connections/`
is owned and groomed by NASty — connections we wrote get a known prefix
(`nasty-*`) so an audit is `nmcli connection show | grep ^nasty-`.

## Proposed apply pipeline: three-state reconciliation

```
desired   = parse NetworkConfig from /var/lib/nasty/networking.json
live      = LiveTopology::snapshot()          (existing, plus NM device state)
owned     = NM connections with prefix "nasty-"
plan      = Plan::compute(desired, owned)     (extends PR2)
plan.execute()                                 (DBus calls to NM)
```

Key changes vs. today:

- `owned` is a precise set: NM connections we authored, identified by
  the `nasty-*` prefix in their connection ID. Everything else (Docker,
  libvirt, wg, ts, plus any user-created NM connection) is external.
- `plan.execute()` is a sequence of DBus calls — `AddConnection`,
  `Update`, `Activate`, `Delete` — instead of `ip` shell-outs. Same Plan
  type from PR2, new executor.
- Plans now include `RemoveLink` ops naturally: anything in `owned` but
  not in `desired` is removed via DBus `Connection.Delete`. This closes
  the gap from PR3's scope cut ("topology removal not in scope").

The runtime+reboot equivalence comes from NM itself: a connection
profile written via DBus is automatically persisted to a keyfile in
`/etc/NetworkManager/system-connections/`. No "write file, also do
something live" — DBus *is* the apply, and persistence happens for free.

## Migration path

Existing installs have:
- `/var/lib/nasty/networking.json` (NASty-managed state, current shape)
- `/etc/nixos/networking.nix` (generated, current shape)
- A NixOS-built system using `networking.bridges.*` etc.

Migration runs once on engine startup, gated by a marker file:

1. Parse `networking.json` from the current shape.
2. Convert to the new layered shape in memory (split L2 / L3).
3. Generate NM connection keyfiles in
   `/etc/NetworkManager/system-connections/nasty-<name>.nmconnection`
   from the new shape, with mode `0600`.
4. Generate a NASty-owned dropin
   `/etc/NetworkManager/conf.d/10-nasty.conf` with the
   `unmanaged-devices` list.
5. Write the new-shape `networking.json` back to disk.
6. Stop generating `/etc/nixos/networking.nix` (replace with a stub
   pointing at this doc).
7. Trigger one `nixos-rebuild switch` to enable NetworkManager and
   disable the legacy `networking.bridges/bonds/vlans/interfaces`. This
   is the *only* time we run a rebuild for network purposes after
   migration.
8. Touch `/var/lib/nasty/.network-migrated-v2`.

The one-shot rebuild in step 7 is itself a risky-network-change — it
flips the active networking backend. It runs inside the equivalent of
PR4's `restore_pending_revert` territory: snapshot prior config first,
apply, validate connectivity, abort to old state if it fails.

For users with hand-edits in `configuration.nix` referencing
`networking.bridges.*` etc., the migration logs a clear warning and
asks them to convert or remove those before proceeding.

## Phases

The work splits into five phases, in dependency order. Each is roughly
the size of one of the #74 follow-up PRs.

**Phase 1 — Layered data model (no behavior change).**
Add the new `Link` / `Address` / `Route` / `Rule` types alongside the
current ones. Add bidirectional converters. Persist the new shape in
parallel under `/var/lib/nasty/networking-v2.json`. Validate-as-a-graph
runs on the new shape and rejects bad input at the API boundary. The
existing apply path still wins.

**Phase 2 — NetworkManager backend (shadow mode).**
Add a NM-DBus executor for the existing `Plan` / `Op` types. Add a
"shadow apply" mode that constructs the DBus calls but doesn't send
them — for diffing against what the existing apply would do. End-to-end
test in a netns harness.

**Phase 3 — Cutover.**
Engine startup migration runs (one-shot `nixos-rebuild switch` to flip
to NetworkManager). New apply path becomes the only apply path.
`networking.json.v1` → archive. Remove the dual-layer code. Remove
dhcpcd; NM's built-in DHCP takes over.

**Phase 4 — Ownership boundary + removal.**
Plan computation becomes truly diff-driven against `owned`. `RemoveLink`
ops are emitted for things removed from desired. Docker / libvirt / wg /
ts ifaces surfaced in the UI as external (read-only, "managed by X").
NM device-state-changed signals piped into the WebUI for live updates.

**Phase 5 — Routing tables, rules, per-service binding.**
Add `Route` / `Rule` execution to the apply pipeline (NM has direct
support for both via connection settings). UI gains a routing panel.
Optional `ServiceBinding` per systemd unit (`SocketBindToDevice=` or
per-service VRF) lands as a separate WebUI affordance. This is the
unlock for "service A egresses via iface 1, service B via iface 2."

Phases 1–3 are foundation; phase 4 is the real ergonomic win (the
model becomes honest about Docker/libvirt); phase 5 is the long-term
feature the model exists to support.

## What this does *not* solve

- **Docker network creation from NASty.** We don't manage Docker's
  networking. We just stop touching it. Letting users create a Docker
  network from the WebUI would be a separate Docker-API integration,
  not a networking model change.
- **libvirt bridge integration.** Same. NASty's VM module already lets
  users attach a VM to a bridge by name; making the WebUI surface
  available bridges with a clear "VM-attach safe" hint is a UI
  improvement, not a model change.
- **WireGuard / Tailscale management.** Both are already managed by
  separate NASty modules. They become external from the network
  layer's perspective; the existing modules continue to manage them.
- **L4 service binding via cgroups / netns.** Network namespaces and
  cgroup-based egress rules are a much bigger feature. The proposed
  model has room for them (a `ServiceBinding` type) but they're not
  in any of the five phases.
- **High availability / VRRP.** Out of scope. If it ever comes up,
  `keepalived` lives outside NASty's owned set.

## Risks

- **NM quirks under heavy bridge use.** NM's bridge support has
  improved significantly in recent versions but historically had rough
  edges with bridge-VLAN filtering and some bond modes. Phase 2's
  netns harness should catch the common cases; an unexpected one
  lands as a real bug in phase 3.
- **Migration surprises.** Step 7 (`nixos-rebuild switch` to flip to
  NM) is itself a risky-network-change in PR4 terms. The migration
  runs in `restore_pending_revert`-equivalent territory and snapshots
  prior config first. Worst case: PR4 rollback catches it.
- **Two code paths during phases 1–2.** Dual-write of v1 + v2 JSON
  means a window where both shapes are kept in sync. Acceptable for
  the duration; collapse aggressively at phase 3.
- **Lock-in to NetworkManager.** If NM gets a deal-breaker bug,
  switching backends after phase 3 is non-trivial. Mitigated by the
  fact that the data model is backend-agnostic — switching to
  systemd-networkd or roll-our-own would mean rewriting only the
  executor, not the model. The Plan/Op intermediate from PR2 is the
  insulation layer.

## Open questions

- **Iface rename support?** NM's `connection.interface-name` is the
  matching key; renaming is awkward. Probably leave to v2.
- **MAC override in the model.** Today MAC inheritance is
  kernel-implicit. NM supports explicit MAC via `cloned-mac-address`.
  Worth keeping `Link.mac: Option<String>` in the model for v1.
- **IPv6 SLAAC privacy address policy.** NM has knobs we don't expose
  today (`ipv6.ip6-privacy`). Defer until someone asks.
- **VRF support.** NM has it via `connection.vrf`. The model has room
  (`Route.table`). Defer until phase 5.
- **Connectivity check for the rollback safety net?** NM's built-in
  connectivity check is a natural complement to PR4's confirm-or-
  rollback — instead of "did the user click in 30s?" we could ask
  "is connectivity-check still passing?" Optional refinement, not
  foundation.
