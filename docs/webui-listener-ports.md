# WebUI listening ports

Issue #965: Settings → General → **WebUI listening ports** allows an unscoped
administrator to select an HTTPS port and an optional HTTP redirect port.
HTTPS cannot be disabled. Ports must be distinct, in 1–65535, and cannot use
the internal engine API (2137) or Caddy admin API (2019).

Applying creates a 120-second transaction. Open the displayed new HTTPS URL,
log in if necessary, and confirm from its Settings page. The browser enables
confirmation only on the candidate HTTPS port. The confirmation RPC checks
the local HTTPS health endpoint before persisting the selection; API callers
must independently verify remote reachability before confirming. If the new
URL cannot be reached, wait for the previous ports to return. SSH is unchanged.
Rollback failures are surfaced and retried rather than silently confirmed.

Generated firewall rules follow the selected ports, retaining source and
interface restrictions for both IPv4 and IPv6. Custom firewall rules are not
removed. HTTP redirects preserve the request path/query, strip the incoming
HTTP port, and bracket IPv6 literals correctly. The HTTPS listener is also
used for app ingress, so app subdomain URLs need the new HTTPS port.

Runtime changes preserve app routes and TLS settings using Caddy ETag/If-Match
updates. Confirmed ports are restored on Caddy restart. Engine restart/reboot
discards an unconfirmed candidate and restores confirmed listeners/rules.
System-update health checks use the confirmed HTTPS port rather than 443.

## Defaults and certificates

Without a saved runtime transaction, the NixOS defaults remain
`services.nasty.webui.port = 443` and `services.nasty.webui.httpPort = 80`.
`httpPort = null` disables the redirect. Both options are still supported;
a stored `/var/lib/nasty/webui-listeners.json` takes precedence thereafter.
The static Caddy hostname uses the configured port explicitly, avoiding an
additional accidental port-443 listener.

Disabling HTTP disables the plaintext redirect, not ACME challenge machinery.
Public HTTP-01/TLS-ALPN-01 challenges still require public ports 80/443; moving
local listeners does not change the certificate authority's destination ports.
Use DNS-01 when those ports are blocked or unavailable. For Internet access,
prefer a VPN and retain restrictive firewall policy for NAS administration.

## Validation

Rust regressions cover validation, preserved Caddy routes, idempotent changes,
optional HTTP, ambiguous configurations, restart recovery, transaction rejection,
and IPv4/IPv6/interface firewall restrictions. Frontend regressions cover input
limits, IPv6 URLs, and confirmation on the candidate HTTPS port.

The Linux appliance smoke test additionally exercises actual redirects,
authenticated WSS on custom ports, firewall updates, preserved SSH, confirmed
Caddy-restart recovery, disabled HTTP, and the real rollback timer.
