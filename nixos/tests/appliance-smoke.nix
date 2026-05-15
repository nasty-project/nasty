# Boots the full NASty appliance (engine + nginx + supporting services) in a
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


    def http_login(password):
        req = urllib.request.Request(
            "http://127.0.0.1:2137/api/login",
            data=json.dumps({"username": "admin", "password": password}).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req) as resp:
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

    # ── Session 3: same RPC through the nginx proxy on 443 ────────
    # Earlier sessions hit the engine on its loopback port directly,
    # which skips nginx entirely.  This one goes wss://127.0.0.1/ws
    # so nginx's WebSocket-upgrade handling is part of what's being
    # asserted.  If a future proxy swap (or nginx config change)
    # breaks Upgrade / Connection forwarding, the engine stays
    # reachable on 2137 but the WebUI breaks — this catches that.
    ws, _ = ws_auth(new_token, url="wss://127.0.0.1/ws", sslopt=SSL_OPTS)
    try:
        health = call(ws, "system.health", 1)
        print("system.health via nginx:", health, file=sys.stderr)
    finally:
        ws.close()
  '';

  pythonWithWs = pkgs.python3.withPackages (ps: [ ps.websocket-client ]);
in

pkgs.testers.runNixOSTest {
  name = "appliance-smoke";

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

    # qemu-vm.nix forces timesyncd off; nasty.nix turns it on. Defer to the
    # VM-test infrastructure since clock sync is irrelevant inside a
    # transient test VM.
    services.timesyncd.enable = lib.mkForce false;

    # The rpc-smoke script needs websocket-client at runtime in the guest.
    environment.systemPackages = [ pythonWithWs ];

    virtualisation.memorySize = 2048;
  };

  testScript = ''
    import json
    import shlex

    machine.start()
    machine.wait_for_unit("nasty-engine.service")

    # ── /health (no auth) ───────────────────────────────────────────
    # Hit the engine directly on its loopback port to skip TLS / nginx.
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

    # ── HTTPS through nginx ────────────────────────────────────────
    # Everything above talked to the engine on 2137 directly, which
    # skips the nginx vhost entirely.  Now exercise the same proxy
    # path the WebUI loads through — `https://127.0.0.1/` with the
    # self-signed cert.  This is what'll catch a future proxy-config
    # regression that leaves the engine reachable on 2137 but breaks
    # the public path.
    machine.wait_until_succeeds("curl -ksS https://127.0.0.1/ -o /dev/null")
    body = machine.succeed("curl -ksS https://127.0.0.1/")
    assert "NASty" in body or "<title>" in body.lower(), (
        f"WebUI response through nginx looks empty/wrong: {body[:200]!r}"
    )

    # The /health endpoint should also be reachable through nginx, not
    # just on the engine loopback.
    health_via_nginx = machine.succeed("curl -ksS https://127.0.0.1/health")
    print(f"=== /health via nginx ===\n{health_via_nginx}")
    assert json.loads(health_via_nginx)["status"] == "ok", (
        f"unexpected health via nginx: {health_via_nginx!r}"
    )

    # ── Security headers on the WebUI response ─────────────────────
    # These are the headers nasty.nix's nginx vhost adds — every one
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
    # rpc-smoke also opens wss://127.0.0.1/ws through nginx as its
    # third session, so the proxy's Upgrade / Connection handling
    # is part of what's being asserted.
    machine.succeed(
        f"${pythonWithWs}/bin/python3 ${rpcSmoke} {shlex.quote(login_obj['token'])}"
    )
  '';
}
