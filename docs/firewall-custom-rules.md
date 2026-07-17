# Firewall: User-Managed Port Rules

Design for user-managed custom port rules on the Firewall page. Requested in
[issue #620](https://github.com/nasty-project/nasty/issues/620) (from
discussion #591): an operator was reduced to `nft add rule inet nasty input
tcp dport <port> accept` by hand — lost on every reboot, because the engine
owns `table inet nasty` and rebuilds it atomically — since there was no
supported way to open a port that isn't tied to a NASty service.

## Scope

**In scope (v1):**
- Open a **single TCP/UDP port or a contiguous range** on the host firewall,
  persisted in engine state, rendered into `table inet nasty` alongside the
  service rules, surviving reboots and rule rebuilds.
- Same optional **source-CIDR and interface restrictions** the service rules
  already support.
- A per-rule **enable/disable toggle** and a required human **label**.
- Validation that **refuses** ports a NASty service owns and **warns** on
  ports a Docker app already publishes.

**Out of scope (deferred):**
- Arbitrary port *sets* / lists in one rule (`{80, 443, 8000-8010}`). A rule
  is one port or one contiguous range; several disjoint ranges are several
  rules.
- Outbound/egress rules. Custom rules apply to host input and matching inbound
  DNAT traffic in the forward hook.
- Protocols other than TCP/UDP.

## Where it matters (and where it doesn't)

- **`network_mode: host` apps** and anything an operator runs directly on the
  host outside NASty's service model — their ports bind on the host and land
  on the default-deny `input` chain, with no UI path to open them today.
- **NASty-managed bridge apps do NOT need this.** Their persisted host ports
  are rendered into the forward allowlist automatically. Other inbound DNAT is
  dropped unless a custom rule explicitly permits the original host port.
- The appliance firewall owns all original-direction inbound DNAT by original
  destination port. NASty-managed app ports are allowed automatically; DNAT
  created by another container or virtualization manager needs an explicit
  custom rule. Reply traffic and non-DNAT forwarding remain untouched.

## Principle: the engine owns the ruleset

Custom rules are just another persisted input to the same atomic rebuild that
already renders service rules. They never escape `table inet nasty`, are never
hand-edited, and survive reboots because the engine re-materializes them on
init — the exact property the hand-rolled `nft add rule` lacked.

## Data model

A new `CustomRule` type in `engine/nasty-system/src/firewall.rs`:

| field | type | meaning |
|-------|------|---------|
| `id` | `String` | Engine-generated opaque UUID — the stable key; survives label edits. |
| `label` | `String` | Required, short, human ("Plex (host mode)"). |
| `transport` | `Transport` | Reuses the existing `Tcp`/`Udp` enum. |
| `from` | `u16` | Low port, `1 ≤ from`. |
| `to` | `u16` | High port, `from ≤ to ≤ 65535`; equals `from` for a single port. |
| `source` | `Option<String>` | Optional source CIDR, same semantics as service rules. |
| `iface` | `Option<String>` | Optional interface restriction, same semantics as service rules. |
| `enabled` | `bool` | The toggle. Disabled rules keep their config but are not rendered. |

`PortSpec` (the single-port type used by service rules) and the derived
service rules are left untouched — custom rules carry their own range and
concerns, so `PortSpec` does not grow a range field that 99% of rules would
never use.

## Persistence

A new `/var/lib/nasty/firewall-custom.json` holding the `Vec<CustomRule>`,
loaded at engine init and held in the serialized `FirewallService` config,
staged and atomically renamed on every successful live-policy mutation. No
secrets are stored in the file.

## Validation (on add / update)

1. **Range sanity.** `1 ≤ from ≤ to ≤ 65535`; `label` non-empty; `transport`
   valid.
2. **Service-owned collision → hard refuse.** The custom `[from,to]` interval,
   matched by transport, is checked against every service rule currently in
   `state.rules`. That set already includes *disabled* protocols (init pushes
   a rule per protocol with `active=false`) and the live iSCSI/NVMe-oF portal
   ports, so it is the full "what a NASty service owns" picture. Any
   intersection is rejected with a message naming the owner — e.g. "tcp/445 is
   managed by SMB — enable SMB to open it." This check lives in the firewall
   module, which owns `state.rules`.
3. **Exact-duplicate custom rule → refuse.** Same transport + from + to +
   source + iface as an existing rule. Overlapping (non-identical) custom
   ranges are allowed — they are additive and harmless.
4. **Docker-published overlap → allow + warn.** If the range intersects a host
   port already allowed for a NASty app, the rule is still created but the
   response explains that it is redundant.

## Engine surface

`FirewallService` exposes three custom-rule methods through the same candidate
state transaction used for service and forward rules:

- `add_custom_rule(input) -> Result<CustomRule, String>` — generate the UUID,
  run validations 1–3, validate/apply nft, persist, then commit in-memory state.
- `update_custom_rule(id, input) -> Result<CustomRule, String>` — find by id,
  re-validate (the duplicate check excludes the rule itself), then commit the
  candidate transaction. The row toggle is an `update` with `enabled` flipped.
- `remove_custom_rule(id) -> Result<(), String>` — drop by id and commit the
  candidate transaction.

RPC arms in `engine/nasty-engine/src/router/system.rs`, registered in
`registry/methods.rs`:

- `system.firewall.custom.add` — params `{ label, transport, from, to,
  source?, iface?, enabled? }` (`enabled` defaults to `true` when omitted);
  returns `{ rule: CustomRule, warnings: [String] }`.
- `system.firewall.custom.update` — params `{ id, label, transport, from, to,
  source?, iface?, enabled }`; returns `{ rule, warnings }`.
- `system.firewall.custom.remove` — params `{ id }`.

All three are **Admin**-role, matching the existing `system.firewall.restrict`
(firewall management is admin territory in a NAS appliance). Admin methods are
accepted by the Admin branch of the authorization gate and are not listed in
`is_operator_allowed`, so the registry-role↔allowlist guard test (which checks
`MethodRole::Operator` methods) does not apply to them.

The `add`/`update` arms compute the Docker-overlap warning at the router layer
(fetching published ports from the app service) and attach it to the response.
The router also synchronizes those ports into `FirewallConfig` for forward-rule
rendering.

## Rendering

The renderer receives one complete `FirewallConfig` snapshot and, after the
service-rule loop, emits one line per **enabled** custom rule:

```
[iifname "<iface>"] [ip saddr <cidr>] <tcp|udp> dport <from>[-<to>] accept # custom:<label>
```

A single port (`from == to`) renders as a bare `dport <from>`; a range renders
as `dport <from>-<to>`. All firewall inputs share one serialized config mutex,
so a candidate cannot mix snapshots from concurrent mutations. The exact same
atomic table-replacement batch is passed to `nft --check` and then `nft -f`.

## Status

`FirewallStatus` gains `custom_rules: Vec<CustomRule>`, filled by `status()`.
It also reports the published app ports held in the same committed config.

## Error handling

- **Validation failure** (range, collision, duplicate) → method error
  (`InvalidParams`) with the pointed message; nothing is persisted, and the UI
  shows it inline.
- **nft validation/application failure** → discard the staged JSON and preserve
  live, persisted, and in-memory state.
- **Persistence failure after live apply** → submit the previous rules as a
  compensating transaction. If that rollback also fails, emit a critical error
  and retain the candidate in memory so status still reflects the live kernel;
  persisted state remains unchanged and the RPC reports failure.
- **Interface disappearance needs no special handling.** The renderer emits
  `iifname "<iface>"` (a per-packet string match), not `iif <index>`.
  `iifname` tolerates an absent interface: the rule simply matches no traffic
  and the `nft -f` load still succeeds. So a custom rule pinned to a
  since-removed interface is **naturally inert — fail-closed by construction**
  — with no widening and no ruleset breakage, and therefore no disable/strip
  logic. (`strip_iface_refs`, the analogous helper for service restrictions,
  is defined but not wired to any live interface-removal event; custom rules
  deliberately do not depend on it.)

## WebUI

`webui/src/routes/firewall/+page.svelte` and `webui/src/lib/types.ts`:

- A new **"Custom port rules"** section listing rows — label, transport,
  port/range, source/interface, an enable toggle, edit, and delete.
- An **add form**: label, transport select, a port field with an optional "to"
  for a range, an optional source CIDR, and an optional interface picker
  (reusing the same interface dropdown source the restrict editor uses).
- **Copy** stating that bridge/published app ports are already open and listed
  read-only above — no custom rule needed — heading off the #591 redundant-
  rule trap directly.
- **Docker-overlap warnings** surface as a non-blocking notice; validation
  refusals as inline errors.
- `types.ts`: add the `CustomRule` interface and extend `FirewallStatus` with
  `custom_rules`.

## Testing

- **Unit** (`firewall.rs`): the validation matrix (range sanity; collision
  refuse against a disabled protocol port, a portal port, and transport-
  sensitivity; exact-duplicate refuse; overlapping-range allow); nft rendering
  (range vs single port; `enabled=false` omitted from the ruleset; a rule with
  an `iface` renders `iifname "<iface>"` so an absent interface is inert).
- **Router layer:** the Docker-overlap warning, tested with a stubbed
  published-ports list (or noted as integration if the harness makes it
  awkward).
- The registry-role↔allowlist guard test already covers the new Admin methods
  implicitly (they are not Operator-role, so it is a correct no-op).

## Follow-ups (not v1)

- Arbitrary port sets / multiple disjoint ranges per rule.
- Egress rules and non-DNAT forwarding policy.
- A "clone from published app port" shortcut for the rare host-mode app that
  genuinely needs a matching rule.
