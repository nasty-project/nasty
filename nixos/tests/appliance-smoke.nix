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
    import subprocess
    import sys
    import time
    import urllib.parse
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


    def recv_response(ws, request_id):
        while True:
            resp = json.loads(ws.recv())
            if resp.get("id") == request_id:
                return resp
            assert "id" not in resp and resp.get("method") == "event", (
                f"unexpected frame while waiting for id={request_id}: {resp!r}"
            )


    def call(ws, method, request_id, params=None):
        req = {"jsonrpc": "2.0", "method": method, "id": request_id}
        if params is not None:
            req["params"] = params
        ws.send(json.dumps(req))
        resp = recv_response(ws, request_id)
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


    def http_json(url):
        with urllib.request.urlopen(url) as resp:
            return json.loads(resp.read())


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

        call(ws, "system.diagnostics.capture", 600, {"enabled": True})
        fs_list = call(ws, "fs.list", 2)
        assert isinstance(fs_list, list), f"fs.list not a list: {fs_list!r}"
        # The test disk is still unformatted at this point.
        assert fs_list == [], f"fs.list expected empty, got {fs_list!r}"
        diagnostics = call(ws, "system.diagnostics.report", 601)
        assert diagnostics["schema_version"] == 1 and diagnostics["detailed_capture"] is True
        assert any(t["operation"] == "fs.list" and t["stage"] == "backend" for t in diagnostics["timings"]), diagnostics
        assert all(set(t) == {"sequence", "operation", "stage", "duration_ms", "outcome"} for t in diagnostics["timings"])
        call(ws, "system.diagnostics.capture", 602, {"enabled": False})
        call(ws, "system.diagnostics.clear", 603)
        assert call(ws, "system.diagnostics.report", 604)["timings"] == []

        # Configurable management listeners: real Caddy, firewall and RPC
        # transaction, with no data/network changes and SSH kept available.
        import http.client
        original_ports = call(ws, "system.webui.get", 500)["confirmed"]
        pending_ports = call(ws, "system.webui.update", 501, {"https_port":8443, "http_port":8080})
        assert pending_ports["pending"]["ports"]["https_port"] == 8443, pending_ports
        for host, expected in [("nas.example:8080", "https://nas.example:8443/health?x=1"), ("[2001:db8::123]:8080", "https://[2001:db8::123]:8443/health?x=1")]:
            conn = http.client.HTTPConnection("127.0.0.1", 8080, timeout=5)
            conn.request("GET", "/health?x=1", headers={"Host":host})
            response = conn.getresponse()
            assert response.status == 308, response.status
            assert response.getheader("Location") == expected, response.getheaders()
            conn.close()
        new_token_ports = http_login(NEW_PW, url="https://127.0.0.1:8443/api/login", ctx=ssl._create_unverified_context())
        new_ws, _ = ws_auth(new_token_ports, url="wss://127.0.0.1:8443/ws", sslopt=SSL_OPTS)
        confirmed_ports = call(new_ws, "system.webui.confirm", 502, {"txn_id":pending_ports["pending"]["txn_id"]})
        new_ws.close()
        assert confirmed_ports["pending"] is None, confirmed_ports
        ipv6_https = http.client.HTTPSConnection("::1", 8443, timeout=5, context=ssl._create_unverified_context())
        ipv6_https.request("GET", "/health")
        ipv6_response = ipv6_https.getresponse()
        assert ipv6_response.status == 200 and json.loads(ipv6_response.read())["status"] == "ok"
        ipv6_https.close()
        firewall = subprocess.check_output(["nft", "list", "table", "inet", "nasty"], text=True)
        assert "tcp dport 8443 accept" in firewall and "tcp dport 8080 accept" in firewall, firewall
        assert "tcp dport 443 accept" not in firewall and "tcp dport 80 accept" not in firewall, firewall
        subprocess.run(["systemctl", "is-active", "--quiet", "sshd.service"], check=True)
        # Confirmed runtime settings survive Caddy-only restart.
        listener_state = "/var/lib/nasty/webui-listeners.json"
        assert subprocess.check_output(["stat", "-c", "%a", listener_state], text=True).strip() == "600"
        assert subprocess.run(["runuser", "-u", "caddy", "--", "test", "-r", listener_state]).returncode != 0
        subprocess.run(["systemctl", "restart", "caddy.service"], check=True)
        assert subprocess.check_output(["systemctl", "show", "caddy.service", "-p", "User", "--value"], text=True).strip() == "caddy"
        assert subprocess.run(["runuser", "-u", "caddy", "--", "test", "-r", listener_state]).returncode != 0
        with urllib.request.urlopen("https://127.0.0.1:8443/health", context=ssl._create_unverified_context()) as response:
            assert json.loads(response.read())["status"] == "ok"
        # HTTP can be disabled without taking HTTPS away.
        disabled = call(ws, "system.webui.update", 503, {"https_port":8443, "http_port":None})
        assert disabled["pending"]["ports"]["http_port"] is None
        assert subprocess.run(["curl", "-fsS", "--max-time", "2", "http://127.0.0.1:8080/health"], capture_output=True).returncode != 0
        call(ws, "system.webui.rollback", 504)
        # Do not confirm this change: exercise the actual timeout worker.
        call(ws, "system.webui.update", 505, {"https_port":8444, "http_port":None})
        deadline = time.monotonic() + 55
        while call(ws, "system.webui.get", 506)["pending"] is not None:
            assert time.monotonic() < deadline, "WebUI listener rollback did not complete"
            time.sleep(2)
        with urllib.request.urlopen("https://127.0.0.1:8443/health", context=ssl._create_unverified_context()) as response:
            assert json.loads(response.read())["status"] == "ok"
        restored = call(ws, "system.webui.update", 507, original_ports)
        call(ws, "system.webui.confirm", 508, {"txn_id":restored["pending"]["txn_id"]})

        # Creating a software device must not require a pre-existing NM
        # device. Exercise the actual RPC/D-Bus path, without touching NICs.
        original_network = call(ws, "system.network.get", 300)["config"]
        bridge_network = json.loads(json.dumps(original_network))
        bridge_network.setdefault("bridges", []).append({
            "name": "br-vm", "members": [],
            "ipv4": {"method": "static", "addresses": ["10.10.30.1/24"], "gateway": None},
            "ipv6": {"method": "disabled", "addresses": [], "gateway": None},
            "stp": False, "forward_delay_s": 0,
        })
        before_routes = subprocess.check_output(["ip", "-4", "route", "show", "default"], text=True)
        result = call(ws, "system.network.update", 301, bridge_network)
        assert not result.get("apply_errors"), result
        if result.get("txn_id"):
            call(ws, "system.network.confirm", 302, {"txn_id": result["txn_id"]})
        for attempt in range(30):
            addr = subprocess.run(["ip", "-4", "addr", "show", "br-vm"], capture_output=True, text=True)
            if "10.10.30.1/24" in addr.stdout:
                break
            time.sleep(1)
        else:
            raise AssertionError("memberless bridge did not activate with its static address")
        assert subprocess.check_output(["ip", "-4", "route", "show", "default"], text=True) == before_routes
        # Keep Docker's generated bridges outside NM ownership while allowing
        # normal user bridge names such as br-vm.
        subprocess.run(["ip", "link", "add", "br-012345abcdef", "type", "bridge"], check=True)
        try:
            for attempt in range(30):
                state = subprocess.run(["nmcli", "-g", "GENERAL.STATE", "device", "show", "br-012345abcdef"], capture_output=True, text=True)
                if state.returncode == 0 and "unmanaged" in state.stdout:
                    break
                time.sleep(1)
            else:
                raise AssertionError(f"Docker bridge unexpectedly managed: {state}")
        finally:
            subprocess.run(["ip", "link", "delete", "br-012345abcdef"], check=True)
        result = call(ws, "system.network.update", 303, original_network)
        assert not result.get("apply_errors"), result
        if result.get("txn_id"):
            call(ws, "system.network.confirm", 304, {"txn_id": result["txn_id"]})

        smart = call(ws, "service.protocol.enable", 20, {"name": "smart"})
        assert smart["enabled"] is True and smart["running"] is True, smart
        smart = call(ws, "service.protocol.disable", 21, {"name": "smart"})
        assert smart["enabled"] is False and smart["running"] is False, smart

        watchdog = call(ws, "system.watchdog.config.get", 22)
        assert watchdog == {
            "max_load_1": 0,
            "max_load_5": 0,
            "max_load_15": 0,
            "min_memory_mib": 0,
            "ping_hosts": [],
        }, watchdog
        watchdog = call(ws, "system.watchdog.config.update", 23, {
            "ping_hosts": ["127.0.0.1"],
        })
        assert watchdog["ping_hosts"] == ["127.0.0.1"], watchdog
        watchdog_status = call(ws, "service.protocol.enable", 24, {"name": "watchdog"})
        assert watchdog_status["enabled"] is True and watchdog_status["running"] is True, watchdog_status
        watchdog_pid = subprocess.check_output([
            "systemctl", "show", "nasty-watchdog.service", "-p", "MainPID", "--value",
        ], text=True).strip()
        assert watchdog_pid not in ("", "0"), watchdog_pid
        time.sleep(12)
        watchdog_protocol = next(
            protocol for protocol in call(ws, "service.protocol.list", 25)
            if protocol["name"] == "watchdog"
        )
        assert watchdog_protocol["running"] is True, watchdog_protocol
        stable_pid = subprocess.check_output([
            "systemctl", "show", "nasty-watchdog.service", "-p", "MainPID", "--value",
        ], text=True).strip()
        restarts = subprocess.check_output([
            "systemctl", "show", "nasty-watchdog.service", "-p", "NRestarts", "--value",
        ], text=True).strip()
        assert stable_pid == watchdog_pid, (watchdog_pid, stable_pid)
        assert restarts == "0", restarts
        watchdog_status = call(ws, "service.protocol.disable", 26, {"name": "watchdog"})
        assert watchdog_status["enabled"] is False and watchdog_status["running"] is False, watchdog_status

        # Apps require their Docker data root on a managed bcachefs pool.
        # Create one through the public RPC so the later apps smoke exercises
        # the same supported setup path as a real appliance.
        ws.settimeout(60)
        created = call(ws, "fs.create", 3, {
            "name": "smoke-pool",
            "devices": [{"path": "/dev/vdb"}],
        })
        ws.settimeout(10)
        assert created["name"] == "smoke-pool", f"fs.create wrong: {created!r}"
        fs_list = call(ws, "fs.list", 4)
        assert any(fs["name"] == "smoke-pool" for fs in fs_list), (
            f"created filesystem missing from fs.list: {fs_list!r}"
        )

        # Disk ownership is reserved even when a VM is stopped. Exercise the
        # RPC safeguards independently of the frontend picker.
        def disk_error(method, request_id, params, expected):
            ws.send(json.dumps({"jsonrpc":"2.0", "id":request_id, "method":method, "params":params}))
            response = recv_response(ws, request_id)
            assert expected in response.get("error", {}).get("message", ""), response

        ws.settimeout(60)
        data_disk = call(ws, "vm.disk.create", 400, {"filesystem":"smoke-pool", "name":"safe-data", "volsize_bytes":67108864})
        vm_a = call(ws, "vm.create", 401, {"name":"disk-owner", "disks":[{"path":data_disk["block_device"]}]})
        vm_b = call(ws, "vm.create", 402, {"name":"disk-recipient", "disks":[]})
        candidates = call(ws, "vm.disk.candidates", 403)
        owned = next(c for c in candidates if c["subvolume"]["name"] == data_disk["name"])
        assert any("disk-owner" in c and "stopped" in c for c in owned["consumers"]), owned
        subprocess.run(["ln", "-s", data_disk["block_device"], "/run/test-disk-alias"], check=True)
        disk_error("vm.update", 404, {"id":vm_b["id"], "disks":[{"path":"/run/test-disk-alias"}]}, "disk-owner")
        disk_error("vm.create", 405, {"name":"duplicate-owner", "disks":[{"path":data_disk["block_device"]}]}, "disk-owner")
        assert call(ws, "vm.get", 406, {"id":vm_b["id"]})["disks"] == []

        csi_disk = call(ws, "vm.disk.create", 407, {"filesystem":"smoke-pool", "name":"reserved-csi", "volsize_bytes":67108864})
        call(ws, "subvolume.set_properties", 408, {"filesystem":"smoke-pool", "name":csi_disk["name"], "properties":{"nasty-csi:managed_by":"nasty-csi", "nasty-csi:pvc_namespace":"lab", "nasty-csi:pvc_name":"database"}})
        reserved = next(c for c in call(ws, "vm.disk.candidates", 409) if c["subvolume"]["name"] == csi_disk["name"])
        assert any("lab/database" in c for c in reserved["consumers"]), reserved
        disk_error("vm.update", 410, {"id":vm_b["id"], "disks":[{"path":csi_disk["block_device"]}]}, "Kubernetes CSI")
        new_disk = call(ws, "vm.disk.create", 411, {"filesystem":"smoke-pool", "name":"recipient-data", "volsize_bytes":67108864})
        call(ws, "vm.update", 412, {"id":vm_b["id"], "disks":[{"path":new_disk["block_device"]}]})
        assert call(ws, "vm.get", 413, {"id":vm_a["id"]})["disks"][0]["path"] == data_disk["block_device"]
        corrupt = "/var/lib/nasty/vms/corrupt.json"
        with open(corrupt, "w") as broken:
            broken.write("{invalid")
        try:
            disk_error("vm.disk.candidates", 414, {}, "cannot verify VM disk usage")
        finally:
            subprocess.run(["rm", corrupt], check=True)
        call(ws, "vm.delete", 415, {"id":vm_a["id"]})
        call(ws, "vm.delete", 416, {"id":vm_b["id"]})
        ws.settimeout(10)

        # Public folder links and the standard-user portal both walk paths
        # through file_boundary. Exercise a real /fs/<name> mount here: unit
        # tests use ordinary temp directories and cannot catch mount-crossing
        # regressions in the descriptor opener.
        subprocess.run(
            ["mkdir", "-p", "/fs/smoke-pool/media/movies"],
            check=True,
        )
        with open("/fs/smoke-pool/media/movies/readme.txt", "w") as marker:
            marker.write("portal-boundary-ok")
        guest = call(ws, "guestshare.create", 5, {
            "paths": ["/fs/smoke-pool/media"],
            "expires_at": None,
            "password": None,
            "max_downloads": None,
            "note": "appliance smoke",
        })
        public_base = (
            "http://127.0.0.1:2137/api/public/share/"
            + urllib.parse.quote(guest["token"], safe="")
        )
        public_meta = http_json(public_base)
        assert public_meta["entries"] == [{
            "root": 0,
            "name": "media",
            "is_dir": True,
            "size": 0,
        }], f"mounted guest folder missing from metadata: {public_meta!r}"
        public_listing = http_json(public_base + "/browse?root=0")
        assert any(
            entry["name"] == "movies" and entry["is_dir"]
            for entry in public_listing["entries"]
        ), f"mounted guest folder cannot be browsed: {public_listing!r}"

        # Unknown method must come back as a JSON-RPC error envelope,
        # not a silent drop.
        ws.send(json.dumps({"jsonrpc": "2.0", "method": "no.such.method", "id": 6}))
        bad = recv_response(ws, 6)
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
        ws.settimeout(60)
        call(ws, "apps.enable", 1, {})
        ws.settimeout(10)
        deadline = _time.monotonic() + 60
        while True:
            status = call(ws, "apps.status", 2)
            if status.get("running"):
                break
            assert _time.monotonic() < deadline, (
                f"apps subsystem never reached running: {status!r}"
            )
            _time.sleep(1)

        # Load only after apps.enable has moved Docker's data root onto the
        # managed pool. Loading earlier would deliberately trip the guard that
        # refuses to replace a populated /var/lib/docker directory.
        subprocess.run(
            ["docker", "load", "-i", "${smokeAppImage}"],
            check=True,
        )

        # Install the smoke app — image is already in docker daemon
        # via the `docker load` step above, so the engine's
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
            "source": "192.0.2.0/24",
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
      watchdog.enable = true;
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
    services.openssh.enable = true;
    services.avahi.enable = true;
    services.smartd.enable = true;
    systemd.services.smartd.wantedBy = lib.mkForce [ ];
    # The VM's synthetic disk has no SMART support. Use a deterministic
    # long-running stand-in so the test can exercise protocol lifecycle.
    systemd.services.smartd.serviceConfig = {
      Type = lib.mkForce "simple";
      ExecStart = lib.mkForce "${pkgs.coreutils}/bin/sleep infinity";
    };

    # The rpc-smoke script needs websocket-client at runtime in the guest.
    environment.systemPackages = [ pythonWithWs ];

    # Also exercise the guard on an explicitly configured mount unit. The
    # engine creates/formats this pool later; noauto avoids a first-boot mount.
    fileSystems."/fs/smoke-pool" = {
      device = "/dev/vdb";
      fsType = "bcachefs";
      options = [ "noauto" ];
    };

    virtualisation.memorySize = 2048;
    virtualisation.emptyDiskImages = [ 2048 ];
  };

  testScript = { nodes, ... }: ''
    import json
    import shlex

    machine.start(allow_reboot=True)

    machine.succeed("nasty-top --version | grep -Fq '0.0.11'")
    machine.succeed("command -v sqlite3 && sqlite3 --version")
    machine.succeed("7zz | grep -Fq '7-Zip'")
    machine.succeed("ncdu --version | grep -Fq 'ncdu 2.11.1'")

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
    machine.succeed("systemctl start smartd.service")
    machine.wait_for_unit("smartd.service")
    machine.succeed(
        "systemctl start nasty-engine.service sshd.service avahi-daemon.service"
    )
    machine.wait_for_unit("nasty-engine.service")
    machine.wait_for_unit("sshd.service")
    machine.succeed("test \"$(systemctl show nasty-engine.service -p Nice --value)\" = -5")
    machine.succeed(
        "test \"$(systemctl show nasty-engine.service -p TimeoutStartUSec --value)\" = infinity"
    )
    machine.succeed(
        "systemctl show sshd.service -p Before --value | tr ' ' '\\n' | grep -Fxq nasty-engine.service"
    )
    machine.succeed(
        "test \"$(systemctl show nasty-engine.service -p CPUSchedulingResetOnFork --value)\" = no"
    )
    machine.succeed(
        "pid=$(systemctl show nasty-engine.service -p MainPID --value); "
        "test \"$(ps -L -o ni= -p \"$pid\" | tr -d ' ' | grep -cx -- '-5')\" -gt 1"
    )

    # SMART stays opt-in on ordinary VMs, but the unit must remain startable
    # for guests with passed-through disks or controllers (#734).
    machine.succeed("test ! -e /etc/systemd/system/multi-user.target.wants/smartd.service")
    machine.fail("systemctl cat smartd.service | grep -q '^ConditionVirtualization='")
    machine.fail("systemctl is-active --quiet smartd.service")
    protocols = json.loads(machine.succeed("cat /var/lib/nasty/protocols.json"))
    assert protocols["smart"] is False, protocols
    assert protocols["watchdog"] is False, protocols
    machine.fail("systemctl is-active --quiet nasty-watchdog.service")
    machine.succeed("test ! -e /etc/systemd/system/multi-user.target.wants/nasty-watchdog.service")
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
    # The dropdown's AliDNS provider must exist in the shipped Caddy binary.
    machine.succeed("${nodes.machine.services.caddy.package}/bin/caddy list-modules | grep -Fx dns.providers.alidns")
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
    # rpc-smoke creates a managed pool, enables apps (which starts Docker),
    # and only then loads the image into the managed Docker data root.
    machine.succeed(
        f"${pythonWithWs}/bin/python3 ${rpcSmoke} {shlex.quote(login_obj['token'])}"
    )
    machine.succeed("grep -Fqx 'ping = 127.0.0.1' /var/lib/nasty/watchdog.conf")
    machine.fail("systemctl is-active --quiet nasty-watchdog.service")

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
    # Exercise Docker DNAT from a non-loopback ingress without relying on the
    # VM test LAN, which NetworkManager may reconfigure during engine startup.
    machine.succeed("ip netns add fwclient")
    machine.succeed("ip link add fw-host type veth peer name fw-client")
    machine.succeed("ip addr add 192.0.2.1/24 dev fw-host")
    machine.succeed("ip link set fw-host up")
    machine.succeed("ip link set fw-client netns fwclient")
    machine.succeed("ip netns exec fwclient ip link set lo up")
    machine.succeed("ip netns exec fwclient ip addr add 192.0.2.2/24 dev fw-client")
    machine.succeed("ip netns exec fwclient ip link set fw-client up")
    machine.wait_until_succeeds(
        "ip netns exec fwclient curl -fsS --max-time 2 http://192.0.2.1:18080/"
    )
    machine.succeed(
        "docker run -d --name unmanaged-smoke -p 18081:80 nasty-smoke-app:test"
    )
    machine.fail(
        "ip netns exec fwclient curl -fsS --max-time 2 http://192.0.2.1:18081/"
    )
    machine.succeed(
        "docker run -d --name restricted-smoke -p 18082:80 nasty-smoke-app:test"
    )
    machine.wait_until_succeeds(
        "ip netns exec fwclient curl -fsS --max-time 2 http://192.0.2.1:18082/"
    )

    # ── Persistent offline-storage maintenance (#948) ─────────────
    # This pool contains the managed Docker data root, so successful normal
    # restoration also proves consumers are not enabled before the pool.
    machine.succeed("nasty-maintenance status | grep -F 'Normal operation'")
    machine.fail("runuser -u nobody -- nasty-maintenance enter --no-reboot")
    machine.fail("test -e /var/lib/nasty/maintenance")
    # A failed durable write must never schedule a reboot.
    machine.succeed("mkdir /var/lib/nasty/maintenance")
    machine.fail("nasty-maintenance enter")
    machine.fail("systemctl list-timers --all --no-legend | grep -F nasty-maintenance-reboot")
    machine.succeed("rmdir /var/lib/nasty/maintenance")
    machine.succeed("nasty-maintenance enter --no-reboot")
    machine.succeed("test -f /var/lib/nasty/maintenance")
    machine.succeed("test $(stat -c %a /var/lib/nasty/maintenance) = 600")
    machine.succeed("nasty-maintenance status | grep -F 'pools may still be mounted'")
    machine.succeed("mountpoint -q /fs/smoke-pool")
    machine.reboot()
    machine.wait_for_unit("nasty-maintenance-access.service")
    machine.wait_for_unit("sshd.service")
    machine.succeed("test -e /run/nasty-maintenance")
    machine.succeed("nft -nn list table inet nasty | grep -F 'udp sport 67 udp dport 68 accept'")
    machine.fail("mountpoint -q /fs/smoke-pool")
    machine.fail("findmnt -rn -t bcachefs")
    for unit in ["nasty-engine", "nasty-metrics", "caddy", "docker", "smartd", "nasty-watchdog"]:
        machine.fail(f"systemctl is-active --quiet {unit}.service")
    # Explicit systemd starts/socket activation must not bypass the guards.
    for unit in ["nasty-engine.service", "docker.service", "docker.socket", "caddy.service"]:
        machine.execute(f"systemctl start {unit}")
        machine.fail(f"systemctl is-active --quiet {unit}")
    machine.execute("systemctl start \"$(systemd-escape --path --suffix=mount /fs/smoke-pool)\"")
    machine.fail("mountpoint -q /fs/smoke-pool")

    # Verify SSH through the default-drop firewall from outside loopback.
    machine.succeed("mkdir -p /root/.ssh; chmod 700 /root/.ssh")
    machine.succeed("ssh-keygen -q -t ed25519 -N \"\" -f /root/maintenance-test-key")
    machine.succeed("cat /root/maintenance-test-key.pub >> /root/.ssh/authorized_keys")
    machine.succeed("chmod 600 /root/.ssh/authorized_keys")
    machine.succeed("ip netns add maintenance-client")
    machine.succeed("ip link add maint-host type veth peer name maint-peer")
    machine.succeed("ip addr add 192.0.2.1/24 dev maint-host")
    machine.succeed("ip link set maint-host up")
    machine.succeed("ip link set maint-peer netns maintenance-client")
    machine.succeed("ip netns exec maintenance-client ip link set lo up")
    machine.succeed("ip netns exec maintenance-client ip addr add 192.0.2.2/24 dev maint-peer")
    machine.succeed("ip netns exec maintenance-client ip link set maint-peer up")
    machine.wait_until_succeeds(
        "ip netns exec maintenance-client ssh -i /root/maintenance-test-key "
        "-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "
        "root@192.0.2.1 nasty-maintenance status | grep -F 'maintenance is active'"
    )
    # Reinstall maintenance SSH rules after a firewall restart too.
    machine.succeed("systemctl restart nftables.service")
    machine.wait_for_unit("nasty-maintenance-access.service")
    machine.succeed("nft list table inet nasty | grep -F 'tcp dport 22 accept'")

    machine.reboot()
    machine.wait_for_unit("nasty-maintenance-access.service")
    machine.fail("mountpoint -q /fs/smoke-pool")
    machine.fail("systemctl is-active --quiet nasty-engine.service")

    machine.succeed("nasty-maintenance exit --no-reboot")
    machine.fail("test -e /var/lib/nasty/maintenance")
    machine.succeed("test -e /run/nasty-maintenance")
    machine.succeed("systemctl daemon-reload")
    machine.succeed("systemctl restart nasty-maintenance-state.service")
    machine.succeed("test -e /run/nasty-maintenance")
    machine.execute("systemctl start nasty-engine.service docker.socket")
    machine.fail("systemctl is-active --quiet nasty-engine.service")
    machine.fail("systemctl is-active --quiet docker.socket")
    machine.fail("mountpoint -q /fs/smoke-pool")
    machine.reboot()
    machine.wait_for_unit("nasty-engine.service")
    machine.fail("test -e /run/nasty-maintenance")
    machine.wait_until_succeeds("mountpoint -q /fs/smoke-pool")
    machine.succeed("test -f /fs/smoke-pool/media/movies/readme.txt")
    machine.wait_for_unit("docker.service")
    machine.wait_until_succeeds("curl -ksS https://127.0.0.1/apps/smoke/ | grep -q nasty-smoke-test-OK")
  '';
}
