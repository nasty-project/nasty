# Two-node AD member-join test: a throwaway Samba AD DC and a NASty
# appliance that joins it, resolves domain users through winbind, and
# reports a healthy trust. This is the only place the whole join flow
# (preflight → net ads join → wbinfo trust → idmap) runs against a real
# KDC — unit tests in nasty-system/src/domain.rs cover the rendering and
# parsing, but never the live Kerberos handshake.
#
# Node IPs come from the NixOS test framework's default net: nodes are
# numbered by the sorted order of `nodes.*`, so `dc` == 192.168.1.1 and
# `nasty` == 192.168.1.2 (see nixpkgs' nixos/lib/testing/network.nix).
{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:

let
  realm = "NASTY.TEST";
  domain = "NASTYAD";
  adminPass = "Passw0rd.123";

  # The AD DC needs a samba built with LDAP + domain-controller support;
  # nixpkgs' default samba is --without-ad-dc.
  sambaDc = pkgs.samba.override {
    enableLDAP = true;
    enableDomainController = true;
  };

  pythonWithWs = pkgs.python3.withPackages (ps: [ ps.websocket-client ]);

  # Self-contained script run inside the nasty guest. Kept in its own file
  # (same reason as appliance-smoke.nix) so a triple-quoted Python block
  # doesn't nest inside the Nix `''` testScript string and get its
  # indentation mangled by the testScript type-checker.
  #
  # It drives the whole member-join flow over the same JSON-RPC path the
  # WebUI uses. Two phases, selected by argv[1], so the test driver can run
  # an out-of-band smbclient auth from the DC while the box is still joined:
  #   join   clear the first-boot password gate, join, assert the domain
  #          user resolves with an idmap UID, search principals, check the
  #          status is healthy, then enable SMB and publish a share
  #          restricted to a domain user.
  #   leave  force a local leave and confirm the unwind.
  adMemberScript = pkgs.writeText "ad-member.py" ''
    import json
    import subprocess
    import sys
    import urllib.request

    import websocket

    REALM = "${realm}"
    DOMAIN = "${domain}"
    ADMIN_PASS = "${adminPass}"
    NEW_PW = "admin-changed-by-ad-test"

    _next_id = 0


    def http_login(password):
        req = urllib.request.Request(
            "http://127.0.0.1:2137/api/login",
            data=json.dumps({"username": "admin", "password": password}).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())["token"]


    def ws_open(token):
        ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=30)
        ws.send(json.dumps({"token": token}))
        auth = json.loads(ws.recv())
        assert auth.get("authenticated") is True, f"WS auth failed: {auth!r}"
        return ws, auth


    def call(ws, method, params=None):
        global _next_id
        _next_id += 1
        req = {"jsonrpc": "2.0", "method": method, "id": _next_id}
        if params is not None:
            req["params"] = params
        ws.send(json.dumps(req))
        resp = json.loads(ws.recv())
        assert resp.get("id") == _next_id, f"id mismatch: {resp!r}"
        assert "error" not in resp, f"{method} returned error: {resp!r}"
        return resp["result"]


    def authed_ws():
        # First boot uses admin/admin with a forced change; the second
        # invocation (leave) logs in with the already-changed password.
        # must_change_password gates the admin-role domain.*/share.* methods.
        for pw in (NEW_PW, "admin"):
            try:
                token = http_login(pw)
            except Exception:
                continue
            ws, auth = ws_open(token)
            if auth.get("must_change_password"):
                call(ws, "auth.change_password",
                     {"username": "admin", "new_password": NEW_PW})
                ws.close()
                token = http_login(NEW_PW)
                ws, auth = ws_open(token)
            assert auth.get("must_change_password") is False, f"gate not cleared: {auth!r}"
            return ws, auth
        raise SystemExit("could not authenticate admin")


    def phase_join(ws):
        # ── Join via the engine API (same path the WebUI uses) ────────
        join = call(ws, "domain.join", {
            "realm": REALM,
            "username": "Administrator",
            "password": ADMIN_PASS,
        })
        assert join["joined"] is True, join
        assert join["trust_ok"] is True, join

        # ── Domain user resolves through winbind/NSS in the idmap range ─
        uid = int(subprocess.check_output(["id", "-u", f"{DOMAIN}\\alice"]).decode().strip())
        assert 100000 <= uid < 1000000, f"idmap uid out of range: {uid}"

        # ── Principal search finds the user ───────────────────────────
        users = call(ws, "domain.user.list", {"prefix": "al"})
        assert any(u["name"].endswith("\\alice") for u in users), users

        # ── Status stays healthy ──────────────────────────────────────
        st = call(ws, "domain.status", {})
        assert st["trust_ok"] and st["dc_reachable"], st

        # ── Publish an SMB share restricted to the domain user ────────
        # Enable the SMB protocol service first — that starts smbd and
        # opens tcp/445 in the firewall (the NixOS-level smb.enable only
        # wires the packages; the engine owns the runtime toggle). Then
        # create a share whose valid_users is a domain principal, proving
        # end to end that winbind auth reaches an engine-managed share.
        call(ws, "service.protocol.enable", {"name": "smb"})
        subprocess.run(["mkdir", "-p", "/fs/adtest"], check=True)
        # World-writable so alice's idmap-mapped uid can create files.
        subprocess.run(["chmod", "0777", "/fs/adtest"], check=True)
        call(ws, "share.smb.create", {
            "name": "adtest",
            "path": "/fs/adtest",
            "valid_users": [f"{DOMAIN}\\alice"],
        })
        print("AD join + domain-user SMB share OK", file=sys.stderr)


    def phase_leave(ws):
        # ── Forced local leave unwinds to not-joined ──────────────────
        call(ws, "domain.leave", {"force": True})
        st = call(ws, "domain.status", {})
        assert st["joined"] is False, st
        print("AD leave unwind OK", file=sys.stderr)


    phase = sys.argv[1] if len(sys.argv) > 1 else "join"
    ws, _auth = authed_ws()
    if phase == "join":
        phase_join(ws)
    elif phase == "leave":
        phase_leave(ws)
    else:
        raise SystemExit(f"unknown phase: {phase}")
    ws.close()
  '';
in

pkgs.testers.runNixOSTest {
  name = "ad-member";

  # ── The throwaway AD domain controller ──────────────────────────
  # No NixOS samba module here — just provision a fresh AD domain at
  # boot and run `samba` in the foreground. Its SAMBA_INTERNAL DNS owns
  # port 53, so systemd-resolved is disabled to keep out of its way.
  nodes.dc = { ... }: {
    networking.firewall.enable = false;
    services.resolved.enable = false;
    environment.systemPackages = [ sambaDc pkgs.dnsutils ];
    virtualisation.memorySize = 2048;

    systemd.services.samba-dc = {
      wantedBy = [ "multi-user.target" ];
      path = [ sambaDc ];
      serviceConfig.Type = "notify";
      serviceConfig.NotifyAccess = "all";
      script = ''
        if [ ! -f /var/lib/samba/private/krb5.conf ]; then
          rm -f /etc/samba/smb.conf
          samba-tool domain provision \
            --realm=${realm} --domain=${domain} \
            --server-role=dc --dns-backend=SAMBA_INTERNAL \
            --adminpass='${adminPass}'
          samba-tool user create alice 'UserPass.123'
        fi
        systemd-notify --ready &
        exec samba --foreground --no-process-group
      '';
    };
  };

  # ── The NASty appliance that joins ──────────────────────────────
  nodes.nasty = { lib, ... }: {
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
      # SMB must stay on — it's what pulls in winbindd, the winbind NSS
      # modules, and the KRB5_CONFIG wiring the join depends on. The
      # other share protocols are irrelevant here and only add boot time.
      smb.enable = true;
      nfs.enable = false;
      iscsi.enable = false;
      nvmeof.enable = false;
    };

    # nasty.nix manages the host network via NetworkManager and
    # force-clears `networking.interfaces` — but NM ignores static
    # `networking.interfaces` and there's no DHCP on the test net, so the
    # node would come up with no route to the DC. Fall back to scripted
    # networking on this node only: NM off, static IP re-asserted above
    # the module's mkForce, and the DC as the resolver (its AD DNS is
    # what answers the realm's SRV records the join preflight looks up).
    networking.networkmanager.enable = lib.mkForce false;
    networking.interfaces = lib.mkOverride 10 {
      eth1.ipv4.addresses = [
        { address = "192.168.1.2"; prefixLength = 24; }
      ];
    };
    networking.nameservers = lib.mkForce [ "192.168.1.1" ];

    # qemu-vm.nix forces timesyncd off; nasty.nix turns it on. Defer to
    # the VM infra — clock sync is irrelevant in a transient test VM, and
    # both guests share the host clock so Kerberos sees ~0 skew.
    services.timesyncd.enable = lib.mkForce false;

    # The join script needs websocket-client at runtime in the guest.
    environment.systemPackages = [ pythonWithWs ];

    virtualisation.memorySize = 2048;
  };

  testScript = ''
    dc.start()
    nasty.start()

    # The DC's AD DNS must be answering the realm's SRV records before the
    # nasty node's join preflight can resolve a domain controller.
    dc.wait_for_unit("samba-dc.service")
    dc.wait_until_succeeds(
        "host -t SRV _ldap._tcp.${pkgs.lib.toLower realm} 127.0.0.1", timeout=120
    )

    nasty.wait_for_unit("nasty-engine.service")
    # The box resolves the realm through the DC (via systemd-resolved) —
    # gate the join on that working end to end.
    nasty.wait_until_succeeds(
        "resolvectl query --type=SRV _ldap._tcp.${pkgs.lib.toLower realm}", timeout=120
    )

    # Phase 1: join, resolve the domain user, and publish an SMB share
    # whose valid_users is the domain principal NASTYAD\alice.
    nasty.succeed("${pythonWithWs}/bin/python3 ${adMemberScript} join")

    # End-to-end proof (spec-mandated): from the DC node, authenticate as the
    # *domain* user against the appliance's SMB share and write a file. This
    # exercises the full winbind auth path — Kerberos/NTLM against the DC,
    # winbind name→uid mapping, and the engine-rendered valid_users ACL.
    nasty.wait_for_open_port(445)
    dc.succeed(
        "smbclient //192.168.1.2/adtest -U 'NASTYAD\\alice%UserPass.123' "
        "-c 'put /etc/hostname adfile'"
    )
    # The written file must be owned by alice's idmap-mapped uid, not a local
    # account — that is the whole point of member mode.
    uid = int(nasty.succeed("stat -c %u /fs/adtest/adfile").strip())
    assert uid >= 100000, f"domain-user file uid not in idmap range: {uid}"

    # Phase 2: force a local leave and confirm the unwind.
    nasty.succeed("${pythonWithWs}/bin/python3 ${adMemberScript} leave")
  '';
}
