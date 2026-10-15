# SvelteKit 3 migration qualification

This migration is **paused, not qualified for merge** as of 2026-10-15.

## Changes

- SvelteKit 2.70.3 → 3.1.0 and adapter-static 3.0.10 → 4.0.0.
- Keep TypeScript 6.0.3, matching Kit 3.1.0's declared peer range.
- Move adapter configuration into the SvelteKit Vite plugin; preserve the
  `index.html` SPA fallback and disabled SSR/prerendering.
- Extend `$app/tsconfig` and explicitly define the existing `$lib` alias in
  Vite and TypeScript, matching Vitest. This avoids a repository-wide import
  rewrite without relying on Kit's removed built-in alias or deprecated
  `kit.alias` configuration.
- Replace removed `$app/stores` imports with reactive `$app/state` reads.
- Replace launcher navigation's `keepFocus`/`noScroll` pair with `reset: false`.
- Declare Kit's Node >=22.17 minimum and refresh the Nix npm dependency hash.

## Declared support versus runtime compatibility

`bits-ui` 2.19.5 depends on `runed` ^0.35.1; `layerchart` 2.8.0 depends on
`runed` ^0.37.1. Both installed runed versions (0.35.1 and 0.37.1) declare
an optional SvelteKit peer of **^2.21.0**, which does not include Kit 3.
Runed 0.37.1 was the latest release checked. Optional means Kit need not be
installed, not that an incompatible installed version satisfies the range.

The repository's existing `npm ci --legacy-peer-deps` install mode bypasses
this conflict. No new override, patched peer declaration, dependency downgrade,
or install-policy change is part of this migration. The peer mismatch alone
is **not evidence of a runtime failure** in our application.

Production module tracing finds 137 ordinary runed module IDs in both unchanged
main (`a8a8cc9d`, Kit 2.70.3) and this migration (Kit 3.1.0), with no bundled
`runed/kit` or `useSearchParams`. The known upstream failure is confined to
that Kit-specific entry point. Our source does not import it directly either.
This substantially narrows the risk, but does not establish upstream support
or guarantee every component/interaction works.

Upstream tracking:

- https://github.com/svecosystem/runed/issues/428 — useSearchParams failure.
- https://github.com/svecosystem/runed/pull/432 — open Kit 3 migration.
- https://github.com/svecosystem/svecodocs/pull/75 — prerequisite docs migration.
- https://github.com/techniq/layerchart/pull/929 — merged package migration.

## Validation and remaining work

Local type checks passed with zero warnings/errors, all 389 WebUI tests passed,
and the static production build passed. `npm audit --audit-level=low` reported
zero vulnerabilities (the previous cookie advisory chain is gone).
`npm ls --all` still fails because of the runed peer mismatch above.

The initial PR checks passed, including Linux/Nix builds, npm hash freshness,
appliance integration and desktop media playback. Android playback has not
been run for this migration.

`tests/media-browser/kit-compatibility.mjs` now adds production-bundle browser
qualification and module tracing. Run it against the migration and unchanged
main as documented in that directory's README. It checks launcher group/query
changes, history and focus restoration, user-role redirects, login labels,
public-share token changes without authenticated requests, dashboard rendering,
dialog Tab confinement/Escape/focus restoration, and real chart tooltip payloads.
All backend data is synthetic; unexpected network/RPC calls fail. These are
not tests of the engine's authorization enforcement or deployed Caddy routing.

The tooltip scenario exposes **`toLocaleTimeString is not a function` on both
unchanged main and Kit 3**, in Chromium and Firefox. The chart wrapper supplies
a label that our IoChart tooltip formatter assumes is a Date. This is a
pre-existing chart integration bug, not demonstrated runed/Kit 3 breakage.
The test remains failing rather than suppressing a baseline failure. Resolve
or explicitly disposition this separate regression before claiming complete
component qualification. The PR stays draft under the user's hold decision.

Independent fix: https://github.com/nasty-project/nasty/pull/991. With that
patch applied in separate local qualification worktrees, all six scenarios
pass on both Chromium and Firefox under both Kit 2 and Kit 3, including actual
sample timestamp labels. The fix is not yet included in this migration branch;
rebase after it merges and rerun qualification before revisiting readiness.

No backend authentication, storage, networking, boot or deployment policy is
changed by this frontend migration.
