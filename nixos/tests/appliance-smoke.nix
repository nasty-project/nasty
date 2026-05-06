# Boots the full NASty appliance (engine + nginx + supporting services) in a
# QEMU VM, waits for the engine to come up, and exercises the HTTP surface:
#   - /health responds before any auth
#   - /api/login with admin/admin succeeds and returns a session token
#   - /api/login with a bad password returns 401
#   - /api/auth/check with the cookie reports authenticated=true
#
# This is the engine ↔ WebUI boundary that unit tests can't reach: real
# systemd unit start, real auth manager (argon2 + lockout DB), real HTTP
# routing, real cookie handshake. Pairs with the JSON-RPC framing tests
# (#29) and the WebSocket client tests (#32) which cover the wire shape on
# either side of this boundary.

{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:

pkgs.testers.runNixOSTest {
  name = "appliance-smoke";

  # websocket-client lets the testScript drive the JSON-RPC dispatch over
  # the same /ws endpoint the WebUI uses.
  extraPythonPackages = ps: [ ps.websocket-client ];

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

    virtualisation.memorySize = 2048;
  };

  testScript = ''
    import json

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

    # ── JSON-RPC over /ws ──────────────────────────────────────────
    # Drive the same dispatch path the WebUI uses: open a WebSocket,
    # auth with the token from /api/login, send a few requests, and
    # check the responses come back correlated by id. This exercises
    # router.rs end-to-end — the part the unit tests deliberately
    # don't reach.
    import shlex
    rpc_script = '''
import json
import sys
import websocket

token = sys.argv[1]

ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=10)
try:
    ws.send(json.dumps({"token": token}))
    auth = json.loads(ws.recv())
    assert auth.get("authenticated") is True, f"WS auth failed: {auth!r}"
    assert auth.get("username") == "admin", f"unexpected user: {auth!r}"

    def call(method, request_id, params=None):
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

    me = call("auth.me", 1)
    assert me["username"] == "admin", f"auth.me wrong: {me!r}"

    health = call("system.health", 2)
    print("system.health:", health, file=sys.stderr)

    fs_list = call("fs.list", 3)
    assert isinstance(fs_list, list), f"fs.list not a list: {fs_list!r}"
    # Fresh appliance with no virtual disks => empty list.
    assert fs_list == [], f"fs.list expected empty, got {fs_list!r}"

    # Unknown method must come back as a JSON-RPC error envelope, not a
    # silent drop.
    ws.send(json.dumps({"jsonrpc": "2.0", "method": "no.such.method", "id": 4}))
    bad = json.loads(ws.recv())
    assert bad.get("id") == 4, f"id mismatch: {bad!r}"
    assert bad.get("error"), f"unknown method should error: {bad!r}"
    print("unknown method error:", bad["error"], file=sys.stderr)
finally:
    ws.close()
'''
    machine.succeed(
        f"python3 -c {shlex.quote(rpc_script)} {shlex.quote(login_obj['token'])}"
    )
  '';
}
