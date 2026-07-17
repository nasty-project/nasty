# Boots the full NASty appliance (engine + Caddy + supporting services) in a
# QEMU VM, waits for the engine to come up, and exercises both halves of the
# WebUI ↔ engine boundary that unit tests can't reach:
#   - HTTP: /health, /api/login (good + bad creds), /api/auth/check
#   - JSON-RPC over /ws: auth.me, system.health, fs.list, unknown-method
#
# Pairs with the JSON-RPC framing tests (#29) and the WebSocket client tests
# (#32) which cover the wire shape on either side of this boundary; this
# proves the engine actually wires up to honour both.

{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:

let
  # Self-contained Python script run inside the guest. Putting it in its own
  # file avoids nesting a triple-quoted Python string inside a Nix `''`
  # string, which mangles indentation under the testScript type-checker.
  rpcSmoke = pkgs.writeText "rpc-smoke.py" ''
    import json
    import ssl
    import sys
    import urllib.request
    import websocket

    initial_token = sys.argv[1]
    NEW_PW = "password-changed-by-smoke-test"

    # Self-signed cert in the test VM — skip verification.
    SSL_OPTS = {"cert_reqs": ssl.CERT_NONE}


    def ws_auth(token, url="ws://127.0.0.1:2137/ws", sslopt=None):
        ws = websocket.create_connection(url, timeout=10, sslopt=sslopt)
        ws.send(json.dumps({"token": token}))
        auth = json.loads(ws.recv())
        assert auth.get("authenticated") is True, f"WS auth failed: {auth!r}"
        assert auth.get("username") == "admin", f"unexpected user: {auth!r}"
        return ws, auth


    def ws_auth_cookie(token):
        # Browser path: session cookie on the upgrade request, no auth message.
        ws = websocket.create_connection(
            "ws://127.0.0.1:2137/ws",
            timeout=10,
            cookie=f"nasty_session={token}",
        )
        auth = json.loads(ws.recv())
        assert auth.get("authenticated") is True, f"WS auth failed: {auth!r}"
        assert auth.get("username") == "admin", f"unexpected user: {auth!r}"
        return ws, auth


    def call(ws, method, request_id, params=None):
        req = {"jsonrpc": "2.0", "method": method, "id": request_id}
        if params is not None:
            req["params"] = params
        ws.send(json.dumps(req))
        resp = json.loads(ws.recv())
        assert resp.get("id") == request_id, (
            f"id mismatch: req={request_id} resp={resp!r}"
        )
        assert "error" not in resp, f"{method} returned error: {resp!r}"
        return resp["result"]


    def http_login(password, url="http://127.0.0.1:2137/api/login", ctx=None):
        req = urllib.request.Request(
            url,
            data=json.dumps({"username": "admin", "password": password}).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, context=ctx) as resp:
            return json.loads(resp.read())["token"]


    # ── Session 1: change the default admin password ──────────────
    # Default admin/admin has must_change_password set, which gates
    # most RPC methods. Change it, then re-login so subsequent calls
    # hit a session that doesn't carry the gate.
    #
    # Both auth paths (token-in-message for non-browsers, cookie-on-upgrade
    # for browsers) must surface must_change_password on the auth response —
    # otherwise the WebUI lands on the dashboard instead of the change-
    # password screen and just silently fails every subsequent RPC.
    ws, auth = ws_auth(initial_token)
    assert auth.get("must_change_password") is True, (
        f"token-path auth missing must_change_password: {auth!r}"
    )
    cookie_ws, cookie_auth = ws_auth_cookie(initial_token)
    cookie_ws.close()
    assert cookie_auth.get("must_change_password") is True, (
        f"cookie-path auth missing must_change_password: {cookie_auth!r}"
    )
    try:
        me = call(ws, "auth.me", 1)
        assert me["username"] == "admin", f"auth.me wrong: {me!r}"
        call(ws, "auth.change_password", 2, {
            "username": "admin",
            "new_password": NEW_PW,
        })
    finally:
        ws.close()

    # ── Session 2: drive a few representative RPCs ────────────────
    new_token = http_login(NEW_PW)
    ws, auth = ws_auth(new_token)
    assert auth.get("must_change_password") is False, (
        f"after change_password, flag should be cleared: {auth!r}"
    )
    try:
        health = call(ws, "system.health", 1)
        print("system.health:", health, file=sys.stderr)

        fs_list = call(ws, "fs.list", 2)
        assert isinstance(fs_list, list), f"fs.list not a list: {fs_list!r}"
        # Fresh appliance with no virtual disks => empty list.
        assert fs_list == [], f"fs.list expected empty, got {fs_list!r}"

        # Unknown method must come back as a JSON-RPC error envelope,
        # not a silent drop.
        ws.send(json.dumps({"jsonrpc": "2.0", "method": "no.such.method", "id": 3}))
        bad = json.loads(ws.recv())
        assert bad.get("id") == 3, f"id mismatch: {bad!r}"
        assert bad.get("error"), f"unknown method should error: {bad!r}"
        print("unknown method error:", bad["error"], file=sys.stderr)
    finally:
        ws.close()

    # ── Session 3: same RPC through the Caddy proxy on 443 ────────
    # Earlier sessions hit the engine on its loopback port directly,
    # which skips Caddy entirely.  This one goes through Caddy end
    # to end so the proxy's WebSocket-upgrade handling is part of
    # what's being asserted.  If a Caddyfile change breaks
    # Upgrade / Connection forwarding, the engine stays reachable
    # on 2137 but the WebUI breaks — this catches that.
    #
    # Sessions are bound to the client IP the engine sees, so a
    # token issued direct to :2137 isn't valid through Caddy (and
    # vice versa).  Login + WS therefore share the proxy path.
    ssl_ctx = ssl.create_default_context()
    ssl_ctx.check_hostname = False
    ssl_ctx.verify_mode = ssl.CERT_NONE
    proxy_token = http_login(
        NEW_PW, url="https://127.0.0.1/api/login", ctx=ssl_ctx
    )
    ws, _ = ws_auth(proxy_token, url="wss://127.0.0.1/ws", sslopt=SSL_OPTS)
    try:
        health = call(ws, "system.health", 1)
        print("system.health via Caddy:", health, file=sys.stderr)
    finally:
        ws.close()

    # ── Session 4: install an app + verify /apps/<name>/ proxy ────
    # Drives the end-to-end apps-ingress path: engine starts Docker,
    # pulls (no-op — image is pre-loaded), creates the container,
    # auto-sets ingress (which POSTs a route to Caddy's admin API
    # at 127.0.0.1:2019).  We then curl `/apps/smoke/` and check
    # the marker string the container serves, which proves:
    #   - the engine's apps lifecycle works end-to-end,
    #   - Caddy's `handle_path` strip-prefix routing works
    #     (request to `/apps/smoke/index.html` reaches the
    #     container as `/index.html`, not `/apps/smoke/index.html`),
    #   - the admin-API route took effect immediately, no reload.
    import time as _time
    ws, _ = ws_auth(new_token)
    try:
        # Start Docker via the engine.  Spawned task waits up to 30s
        # for the daemon, so we poll apps.status afterwards.
        call(ws, "apps.enable", 1, {})
        deadline = _time.monotonic() + 60
        while True:
            status = call(ws, "apps.status", 2)
            if status.get("running"):
                break
            assert _time.monotonic() < deadline, (
                f"apps subsystem never reached running: {status!r}"
            )
            _time.sleep(1)

        # Install the smoke app — image is already in docker daemon
        # via the `docker load` step in testScript, so the engine's
        # `pull_image` step is a no-op cache hit.
        call(ws, "apps.install", 3, {
            "name": "smoke",
            "image": "nasty-smoke-app:test",
            "ports": [{
                "name": "http",
                "container_port": 80,
                "host_port": 18080,
                "protocol": "tcp",
            }],
        })
        call(ws, "system.firewall.custom.add", 4, {
            "label": "restricted external forward",
            "transport": "tcp",
            "from": 18082,
            "to": 18082,
            "source": "192.168.1.0/24",
            "enabled": True,
        })
    finally:
        ws.close()
  '';

  pythonWithWs = pkgs.python3.withPackages (ps: [ ps.websocket-client ]);

  # Tiny Docker image used by the apps-ingress smoke step.  Boots
  # busybox httpd against a static `/www/index.html` whose contents
  # are a known marker string the test asserts on, so we know the
  # request actually reached the container — and through which path.
  smokeAppImage = pkgs.dockerTools.buildImage {
    name = "nasty-smoke-app";
    tag = "test";
    copyToRoot = pkgs.buildEnv {
      name = "nasty-smoke-app-root";
      paths = [
        pkgs.busybox
        (pkgs.writeTextDir "www/index.html" "nasty-smoke-test-OK")
      ];
      pathsToLink = [ "/bin" "/www" ];
    };
    config = {
      Cmd = [ "httpd" "-f" "-p" "80" "-h" "/www" ];
    };
  };
in

pkgs.testers.runNixOSTest {
  name = "appliance-smoke";

  nodes.client = { pkgs, ... }: {
    environment.systemPackages = [ pkgs.curl ];
  };

  nodes.machine = { lib, ... }: {
    imports = [
      ../modules/bcachefs.nix
      ../modules/linuxquota.nix
      ../modules/nasty.nix
    ];
    _module.args = {
      inherit nasty-engine nasty-webui nasty-bcachefs-tools;
      nasty-version = "test";
    };

    services.nasty = {
      enable = true;
      engine.package = nasty-engine;
      webui.package = nasty-webui;
      # Share-protocol services aren't relevant for the API smoke and add
      # boot time + kernel module dependencies. The engine API is happy
      # without them — it just won't be able to actually create shares.
      nfs.enable = false;
      smb.enable = false;
      iscsi.enable = false;
      nvmeof.enable = false;
    };

    # Docker is needed for the apps-ingress assertion.  The engine's
    # `apps.enable` RPC starts docker.service itself, but the unit has
    # to exist — wantedBy is cleared so it doesn't race nasty-engine
    # on boot.
    virtualisation.docker.enable = true;
    systemd.services.docker.wantedBy = lib.mkForce [ ];
    systemd.sockets.docker.wantedBy = lib.mkForce [ ];

    # Stage the smoke-app image tarball into the VM's Nix store so
    # `docker load` finds it without ever needing network access.
    system.extraDependencies = [ smokeAppImage ];

    # qemu-vm.nix forces timesyncd off; nasty.nix turns it on. Defer to the
    # VM-test infrastructure since clock sync is irrelevant inside a
    # transient test VM.
    services.timesyncd.enable = lib.mkForce false;
    services.avahi.enable = true;

    # The rpc-smoke script needs websocket-client at runtime in the guest.
    environment.systemPackages = [ pythonWithWs ];

    virtualisation.memorySize = 2048;
  };

  testScript = ''
    import json
    import shlex

    machine.start()
    client.start()

    # nftables must establish a default-drop baseline independently of the
    # engine. Restart nftables as separate stop/start operations so PartOf stops
    # the engine without restarting it; inspect the baseline, then start the
    # engine and verify it installs its dynamic management rules before ready.
    machine.wait_for_unit("nftables.service")
    machine.succeed("systemctl stop nftables.service")
    machine.succeed("systemctl start nftables.service")
    baseline = machine.succeed("nft list table inet nasty")
    assert "policy drop" in baseline, f"missing fail-closed baseline: {baseline}"
    assert "ct direction original ct status dnat drop" in baseline, (
        f"baseline does not block DNAT forwarding: {baseline}"
    )
    assert "dport 443" not in baseline, f"baseline unexpectedly exposes WebUI: {baseline}"
    machine.succeed(
        "systemctl start nasty-engine.service sshd.service avahi-daemon.service"
    )
    machine.wait_for_unit("nasty-engine.service")
    machine.wait_for_unit("sshd.service")
    machine.wait_for_unit("avahi-daemon.service")

    dynamic_firewall = machine.succeed("nft list table inet nasty")
    assert "tcp dport 443 accept" in dynamic_firewall, (
        f"engine did not install dynamic WebUI policy: {dynamic_firewall}"
    )
    machine.fail("systemctl reload nftables.service")
    after_rejected_reload = machine.succeed("nft list table inet nasty")
    assert "tcp dport 443 accept" in after_rejected_reload, (
        f"rejected nftables reload discarded dynamic policy: {after_rejected_reload}"
    )
    machine.succeed("systemctl restart nftables.service")
    for unit in [
        "nasty-engine.service",
        "nasty-metrics.service",
        "caddy.service",
        "sshd.service",
        "avahi-daemon.service",
    ]:
        machine.wait_for_unit(unit)
    after_restart = machine.succeed("nft list table inet nasty")
    assert "tcp dport 443 accept" in after_restart, (
        f"nftables restart did not restore dynamic policy: {after_restart}"
    )

    # A valid batch whose final command references a missing chain must fail as
    # a whole. The named sentinel in the old table proves the leading destroy
    # command was rolled back by nft's netlink transaction.
    machine.succeed("nft add counter inet nasty transaction_sentinel")
    machine.fail(
        "printf 'destroy table inet nasty\\nadd table inet nasty\\n"
        "add rule inet nasty missing_chain counter\\n' | nft --file -"
    )
    machine.succeed("nft list counter inet nasty transaction_sentinel")

    # ── /health (no auth) ───────────────────────────────────────────
    # Hit the engine directly on its loopback port to skip TLS / Caddy.
    machine.wait_until_succeeds("curl -fsS http://127.0.0.1:2137/health")
    health = machine.succeed("curl -fsS http://127.0.0.1:2137/health")
    print(f"=== /health ===\n{health}")
    health_obj = json.loads(health)
    assert health_obj["status"] == "ok", f"unexpected health: {health_obj!r}"

    # ── /api/login with default admin/admin ────────────────────────
    login = machine.succeed(
        "curl -fsS -c /tmp/cookies.txt "
        "-X POST http://127.0.0.1:2137/api/login "
        "-H 'Content-Type: application/json' "
        "-d '{\"username\":\"admin\",\"password\":\"admin\"}'"
    )
    print(f"=== /api/login response ===\n{login}")
    login_obj = json.loads(login)
    assert "token" in login_obj and login_obj["token"], (
        f"login response missing token: {login_obj!r}"
    )

    # The Set-Cookie header should have landed in the cookie jar too.
    jar = machine.succeed("cat /tmp/cookies.txt")
    assert "nasty_session" in jar, f"session cookie not set: {jar!r}"

    # ── /api/login with bad credentials ────────────────────────────
    # curl -f exits non-zero on 4xx, so machine.fail is the assertion.
    machine.fail(
        "curl -fsS -X POST http://127.0.0.1:2137/api/login "
        "-H 'Content-Type: application/json' "
        "-d '{\"username\":\"admin\",\"password\":\"wrong-on-purpose\"}'"
    )

    # ── /api/auth/check with the cookie ────────────────────────────
    # The handler returns 200 OK with an empty body when the session is
    # valid. curl -f exits non-zero on 4xx, so a successful exit here is
    # the assertion that the cookie was accepted.
    machine.succeed(
        "curl -fsS -b /tmp/cookies.txt "
        "http://127.0.0.1:2137/api/auth/check"
    )

    # Same endpoint without a cookie should be rejected.
    machine.fail("curl -fsS http://127.0.0.1:2137/api/auth/check")

    # ── HTTPS through Caddy ────────────────────────────────────────
    # Everything above talked to the engine on 2137 directly, which
    # skips the Caddy vhost entirely.  Now exercise the same proxy
    # path the WebUI loads through — `https://127.0.0.1/` with the
    # self-signed cert.  This is what'll catch a future proxy-config
    # regression that leaves the engine reachable on 2137 but breaks
    # the public path.
    machine.wait_until_succeeds("curl -ksS https://127.0.0.1/ -o /dev/null")
    body = machine.succeed("curl -ksS https://127.0.0.1/")
    # SvelteKit hydrates title / branding in JS so the initial HTML
    # body doesn't carry app-specific strings — assert only that we
    # got real HTML back through the proxy, not an error page.
    body_lc = body.lstrip().lower()
    assert body_lc.startswith("<!doctype html>") or "<html" in body_lc, (
        f"WebUI response through Caddy doesn't look like HTML: {body[:200]!r}"
    )

    # The /health endpoint should also be reachable through Caddy, not
    # just on the engine loopback.
    health_via_caddy = machine.succeed("curl -ksS https://127.0.0.1/health")
    print(f"=== /health via Caddy ===\n{health_via_caddy}")
    assert json.loads(health_via_caddy)["status"] == "ok", (
        f"unexpected health via Caddy: {health_via_caddy!r}"
    )

    # ── Security headers on the WebUI response ─────────────────────
    # These are the headers nasty.nix's Caddy vhost adds — every one
    # of them is a hardening assertion we don't want to silently lose
    # in a future proxy refactor.  Match prefix-only so a Caddy port
    # that ships slightly different values (e.g. `max-age=63072000`
    # instead of `31536000`) still passes if the header is present.
    headers = machine.succeed("curl -ksS -D - https://127.0.0.1/ -o /dev/null")
    print(f"=== response headers ===\n{headers}")
    lower = headers.lower()
    for required in (
        "strict-transport-security:",
        "x-content-type-options:",
        "x-frame-options:",
        "referrer-policy:",
        "content-security-policy:",
    ):
        assert required in lower, (
            f"missing security header {required!r} in:\n{headers}"
        )

    # ── JSON-RPC over /ws ──────────────────────────────────────────
    # Drive the same dispatch path the WebUI uses: open a WebSocket,
    # auth with the token from /api/login, send a few requests, check
    # responses come back correlated by id. Exercises router.rs
    # end-to-end — the part unit tests deliberately don't reach.
    # rpc-smoke also opens wss://127.0.0.1/ws through Caddy as its
    # third session, and (session 4) installs a Docker app whose
    # /apps/<name>/ ingress we then assert against below.
    #
    # Docker is needed before we can `docker load` the smoke image.
    # The engine's `apps.enable` RPC would start it too, but the
    # load step runs from the testScript (host-side), so we bring
    # it up explicitly first.  Once up, rpc-smoke's `apps.enable`
    # is a no-op systemctl restart.
    machine.succeed("systemctl start docker.socket")
    machine.wait_for_unit("docker.socket")
    machine.wait_until_succeeds("docker info >/dev/null 2>&1")
    machine.succeed("docker load -i ${smokeAppImage}")
    machine.succeed(
        f"${pythonWithWs}/bin/python3 ${rpcSmoke} {shlex.quote(login_obj['token'])}"
    )

    # ── /apps/<name>/ ingress through Caddy ───────────────────────
    # rpc-smoke just installed an app called "smoke" mapped to
    # host port 18080.  The engine wrote a `location /apps/smoke/`
    # block via the Caddy admin API at 127.0.0.1:2019.
    # Curl both the bare route and a subpath: the marker string in
    # the response proves Caddy's `handle_path` strip-prefix
    # http://host:port/` actually strips, not just appends.
    machine.wait_until_succeeds(
        "curl -ksS https://127.0.0.1/apps/smoke/ | grep -q nasty-smoke-test-OK"
    )
    bare = machine.succeed("curl -ksS https://127.0.0.1/apps/smoke/")
    print(f"=== /apps/smoke/ ===\n{bare}")
    assert "nasty-smoke-test-OK" in bare, f"unexpected ingress body: {bare!r}"

    subpath = machine.succeed("curl -ksS https://127.0.0.1/apps/smoke/index.html")
    assert "nasty-smoke-test-OK" in subpath, (
        f"path-strip regression — /apps/smoke/index.html didn't reach "
        f"the container's /index.html: {subpath!r}"
    )

    forward_policy = machine.succeed("nft list table inet nasty")
    assert "ct original proto-dst 18080 accept" in forward_policy, (
        f"managed app port missing from Docker forward policy: {forward_policy}"
    )
    client.wait_until_succeeds("curl -fsS --max-time 2 http://machine:18080/")
    machine.succeed(
        "docker run -d --name unmanaged-smoke -p 18081:80 nasty-smoke-app:test"
    )
    client.fail("curl -fsS --max-time 2 http://machine:18081/")
    machine.succeed(
        "docker run -d --name restricted-smoke -p 18082:80 nasty-smoke-app:test"
    )
    client.wait_until_succeeds("curl -fsS --max-time 2 http://machine:18082/")
  '';
}
