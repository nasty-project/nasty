# SvelteKit 3 migration qualification

This migration is **blocked, not qualified for merge** as of 2026-10-15.

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

## Support blocker

`bits-ui` 2.19.5 depends on `runed` ^0.35.1; `layerchart` 2.8.0 depends on
`runed` ^0.37.1. Both installed runed versions (0.35.1 and 0.37.1) declare
an optional SvelteKit peer of **^2.21.0**, which does not include Kit 3.
Runed 0.37.1 was the latest release checked. Optional means Kit need not be
installed, not that an incompatible installed version satisfies the range.

The repository's existing `npm ci --legacy-peer-deps` install mode bypasses
this conflict. No new override, patched peer declaration, dependency downgrade,
or install-policy change is part of this migration. Passing tests does not
establish upstream support. Resolve this compatibility decision before merge.

## Validation and remaining work

Local type checks passed with zero warnings/errors, all 389 WebUI tests passed,
and the static production build passed. `npm audit --audit-level=low` reported
zero vulnerabilities (the previous cookie advisory chain is gone).
`npm ls --all` still fails because of the runed peer mismatch above.

CI must validate Linux/Nix builds, the refreshed npm hash, appliance integration
and desktop media playback. Before marking ready, also qualify authenticated
navigation (launcher groups, back/forward, role redirects and query-driven
pages) and public-share token changes in the browser. Existing unit/build
success alone is not an end-to-end authentication or navigation test. Android
playback is available through the existing manual media workflow.

No backend authentication, storage, networking, boot or deployment policy is
changed by this frontend migration.
