# Two-node AD test, DC side: a NASty box provisions a brand-new Active
# Directory domain via the engine API (real `nasty.nix` samba-dc.service,
# real `dc.provision` RPC — not a throwaway script-unit) and a second NASty
# box joins it with the shipped member flow (#627). This is the fleet
# money-shot for #20: it proves the two roles this feature ships —
# "NASty hosts a domain" and "NASty joins a domain" — actually interop,
# end to end, over the wire.
#
# ad-member.nix is the harness template this file cribs from: module
# imports, `_module.args`, the JSON-RPC-over-websocket driver-script
# pattern (login → bearer → call), and the out-of-band smbclient check
# from the other node. The one structural difference: ad-member's DC side
# is a throwaway samba-tool script-unit standing in for "some AD DC";
# here BOTH boxes are full NASty appliances, so the DC side exercises the
# real engine-owned unit + provisioning path this feature actually ships.
#
# Node IPs come from the NixOS test framework's default net: nodes are
# numbered by the sorted order of `nodes.*`, so `dcbox` == 192.168.1.1 and
# `member` == 192.168.1.2 (see nixpkgs' nixos/lib/testing/network.nix —
# same mechanism ad-member.nix documents for its `dc`/`nasty` nodes).
#
# This is samba-dc.service's first live exercise (unit tests cover the
# argv/render/parse logic in nasty-system/src/dc.rs, but never a real
# `samba-tool domain provision` + service start). Two named risk areas if
# this VM run disproves an assumption the code makes:
#   (a) Type=notify readiness — samba-dc.service's ExecStart execs `samba
#       --foreground` directly with no wrapper script (unlike ad-member's
#       throwaway DC, which manually fires `systemd-notify --ready`).
#       If samba's own sd_notify support never signals READY=1,
#       `systemctl start samba-dc.service` (called synchronously inside
#       dc.provision) fails at its own TimeoutStartSec, well before this
#       test's 60s service_healthy poll or 300s RPC socket timeout would
#       matter. Fallback: switch the unit to Type=simple and poll
#       `samba-tool` reachability from dc.status instead.
#   (b) provision-vs-config-path — dc.rs's samba-tool invocations already
#       pass `--configfile=/etc/samba/smb.dc.conf` (DC_CONF_PATH) directly
#       to `domain provision`, so this should already write the config
#       where the module's ExecStart expects it. If samba-tool refuses to
#       write there, the fallback is `--targetdir` into a scratch dir with
#       the generated smb.conf moved to DC_CONF_PATH afterward.
{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:

let
  realm = "NASTYDC.LAN";
  # derive_workgroup(realm) (nasty-system/src/domain.rs): first label,
  # NetBIOS-truncated to 15 chars, uppercased. "NASTYDC" needs no
  # truncation — spelling it out here (rather than deriving it in Nix)
  # keeps this file honest about what the wbinfo domain prefix actually
  # has to be.
  workgroup = "NASTYDC";
  adminPass = "Passw0rd.123";
  alicePass = "UserPass.123";
  # Fixed and deterministic rather than "the test net's gateway" — this
  # isolated two-node vlan has no router/gateway node at all. dc.provision
  # only validates that dns_forwarder parses as an IP address (it's
  # written into smb.dc.conf, never dialled during provision), so an
  # unreachable-but-well-formed address is fine and keeps the testparm
  # assertion below deterministic.
  dnsForwarder = "192.168.1.254";

  pythonWithWs = pkgs.python3.withPackages (ps: [ ps.websocket-client ]);

  # Self-contained script run inside each guest, same reason ad-member.nix
  # keeps its driver in its own file (a triple-quoted Python block nested
  # inside the Nix `''` testScript string gets its indentation mangled by
  # the testScript type-checker). One file, two phases selected by
  # argv[1] — `provision` runs on dcbox, `join` runs on member — sharing
  # the login/websocket/call boilerplate the way ad-member's join/leave
  # phases do.
  adDcScript = pkgs.writeText "ad-dc.py" ''
    import json
    import subprocess
    import sys
    import time
    import urllib.request

    import websocket

    REALM = "${realm}"
    WORKGROUP = "${workgroup}"
    ADMIN_PASS = "${adminPass}"
    ALICE_PASS = "${alicePass}"
    DNS_FORWARDER = "${dnsForwarder}"
    NEW_PW = "admin-changed-by-ad-dc-test"

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
        # `dc.provision` runs `samba-tool domain provision` synchronously
        # inside the RPC call — LDAP database init, Kerberos realm setup,
        # sysvol — much heavier than a plain domain.join. Give the socket
        # a generous read timeout so a slow CI runner doesn't trip a
        # client-side timeout on an otherwise-successful call.
        ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=300)
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
        # First boot uses admin/admin with a forced change, same gate
        # ad-member.py's authed_ws() clears — every fresh NASty box starts
        # here regardless of which role it ends up playing.
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


    def phase_provision(ws):
        # ── Provision a brand-new domain via the engine API ────────────
        # Note for future readers: samba-tool domain provision wipes
        # /var/lib/samba/private before writing the AD database there —
        # any local SMB passdb users on this box are gone after this
        # call. Deliberate (the DC replaces local SMB auth entirely);
        # nothing local is configured on this fresh test box, so there's
        # nothing to assert here beyond flagging it for the next reader.
        prov = call(ws, "dc.provision", {
            "realm": REALM,
            "admin_password": ADMIN_PASS,
            "dns_forwarder": DNS_FORWARDER,
        })
        assert prov["status"]["hosting"] is True, prov
        # A static-IP *warning* is expected here — this VM's address isn't
        # configured through NASty's own network.* RPC (networking.json
        # never gets written), so the precondition can't confirm a static
        # address and downgrades Fail to Warn (dc.rs::static_ip_check).
        # Provision must still succeed with a non-empty warnings array.
        assert isinstance(prov["warnings"], list), prov

        # ── Poll until samba-dc.service reports healthy ─────────────────
        healthy_status = None
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            st = call(ws, "dc.status")
            if st.get("service_healthy"):
                healthy_status = st
                break
            time.sleep(1)
        assert healthy_status is not None, "samba-dc.service did not report healthy within 60s"
        assert healthy_status["hosting"] is True, healthy_status
        assert healthy_status["realm"] == REALM, healthy_status
        assert healthy_status["workgroup"] == WORKGROUP, healthy_status
        assert healthy_status["dns_forwarder"] == DNS_FORWARDER, healthy_status

        # ── Create a domain user, confirm it shows up in the listing ────
        call(ws, "dc.user.create", {"name": "alice", "password": ALICE_PASS})
        users = call(ws, "dc.user.list")
        assert any(u["name"] == "alice" for u in users), users

        # dc.backup is intentionally not exercised here: like ad-member's
        # harness, this one sets up no /fs pool, and dc.backup jails its
        # target under /fs (dc.rs::validate_backup_dest). The jail and the
        # samba-tool backup call itself are unit-tested; fabricating a
        # pool just to exercise this one RPC isn't worth the added
        # boot-time/complexity here.
        print("DC provisioned + healthy + alice created", file=sys.stderr)


    def phase_join(ws):
        # ── Join via the engine API — the shipped member flow (#627),
        # unchanged ────────────────────────────────────────────────────
        join = call(ws, "domain.join", {
            "realm": REALM,
            "username": "Administrator",
            "password": ADMIN_PASS,
        })
        assert join["joined"] is True, join
        assert join["trust_ok"] is True, join

        st = call(ws, "domain.status")
        assert st["joined"] is True, st
        assert st["trust_ok"] is True, st

        # winbindd is started directly by domain.join (not gated behind
        # the smb protocol toggle) — wbinfo should resolve the domain
        # user right away. `wbinfo -i` mimics getent passwd's
        # colon-delimited line; field index 2 is the uid.
        out = subprocess.run(
            ["wbinfo", "-i", f"{WORKGROUP}\\alice"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        uid = int(out.split(":")[2])
        assert 100000 <= uid < 1000000, f"idmap uid out of range: {uid} (wbinfo said: {out!r})"

        # Start smbd (port 445) — needed for the out-of-band smbclient
        # probe from dcbox that follows. Build-time-vs-runtime split: the
        # module's smb.enable pulled in winbind/KRB5 wiring already, but
        # the SMB daemons themselves only start via the engine's runtime
        # protocol toggle (same as ad-member.py's phase_join).
        call(ws, "service.protocol.enable", {"name": "smb"})
        print(f"domain join + wbinfo resolve OK (uid={uid})", file=sys.stderr)


    phase = sys.argv[1] if len(sys.argv) > 1 else "provision"
    ws, _auth = authed_ws()
    if phase == "provision":
        phase_provision(ws)
    elif phase == "join":
        phase_join(ws)
    else:
        raise SystemExit(f"unknown phase: {phase}")
    ws.close()
  '';

in

pkgs.testers.runNixOSTest {
  name = "ad-dc";

  # ── The NASty box that hosts the domain ───────────────────────────
  # Both nodes below are full NASty appliances (same imports/_module.args
  # as ad-member.nix's "nasty" node), and both hit the same
  # NetworkManager-vs-test-network snag it documents: nasty.nix hands
  # networking to NetworkManager and force-clears `networking.interfaces`,
  # but there's no DHCP on this test net, so scripted networking is
  # reasserted on both (NM off, static IP above the module's mkForce).
  # ad-member only needed this workaround on its single "nasty" node;
  # here it applies to both, since there is no "nasty.nix doesn't apply"
  # side of this topology.
  nodes.dcbox = { lib, ... }: {
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
      # SMB must stay on: it's what pulls in the DC-capable samba build
      # (sambaAds) that samba-dc.service execs. The other share protocols
      # are irrelevant here and only add boot time.
      smb.enable = true;
      nfs.enable = false;
      iscsi.enable = false;
      nvmeof.enable = false;
    };

    networking.networkmanager.enable = lib.mkForce false;
    networking.interfaces = lib.mkOverride 10 {
      eth1.ipv4.addresses = [
        { address = "192.168.1.1"; prefixLength = 24; }
      ];
    };
    # dc.provision writes its own resolved drop-in once the DC is up
    # (DNS=127.0.0.1, samba's internal DNS) — nothing needed here
    # upstream of that; this isolated test net has no real resolver.

    # qemu-vm.nix forces timesyncd off; nasty.nix turns it on. Defer to
    # the VM infra — clock sync is irrelevant in a transient test VM,
    # and both guests share the host clock so Kerberos sees ~0 skew.
    services.timesyncd.enable = lib.mkForce false;

    # The driver script needs websocket-client at runtime in the guest.
    environment.systemPackages = [ pythonWithWs ];

    virtualisation.memorySize = 3072;
  };

  # ── The NASty box that joins it ───────────────────────────────────
  nodes.member = { lib, ... }: {
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

    networking.networkmanager.enable = lib.mkForce false;
    networking.interfaces = lib.mkOverride 10 {
      eth1.ipv4.addresses = [
        { address = "192.168.1.2"; prefixLength = 24; }
      ];
    };
    # dcbox owns DNS for the realm once provisioned — the join
    # preflight's SRV lookups resolve through it.
    networking.nameservers = lib.mkForce [ "192.168.1.1" ];

    services.timesyncd.enable = lib.mkForce false;

    environment.systemPackages = [ pythonWithWs ];

    virtualisation.memorySize = 2048;
  };

  testScript = ''
    dcbox.start()
    member.start()

    dcbox.wait_for_unit("nasty-engine.service")

    # ── Phase 1 (dcbox): provision the domain, create a user ──────────
    dcbox.succeed("${pythonWithWs}/bin/python3 ${adDcScript} provision")

    # DC's own DNS must answer the realm's SRV records locally before the
    # member's join preflight can find it over the network.
    dcbox.wait_until_succeeds(
        "resolvectl query --type=SRV _ldap._tcp.${pkgs.lib.toLower realm}", timeout=60
    )

    # Pin the strip-samba's-line fix (#20 review finding): the
    # operator-supplied dns_forwarder must be the config's EFFECTIVE
    # value, not silently overridden by samba-tool provision's own
    # resolv.conf-derived line (smb.conf is last-value-wins).
    forwarder = dcbox.succeed(
        "testparm -s --parameter-name='dns forwarder' /etc/samba/smb.dc.conf 2>/dev/null | tail -1"
    ).strip()
    assert forwarder == "${dnsForwarder}", (
        f"effective dns forwarder mismatch: got {forwarder!r}, want ${dnsForwarder}"
    )

    # ── Phase 2 (member): join the domain dcbox now hosts ──────────────
    member.wait_for_unit("nasty-engine.service")
    member.wait_until_succeeds(
        "resolvectl query --type=SRV _ldap._tcp.${pkgs.lib.toLower realm}", timeout=120
    )
    member.succeed("${pythonWithWs}/bin/python3 ${adDcScript} join")

    # ── Out-of-band proof (spec-mandated money shot): from dcbox,
    # authenticate as the domain user against member's IPC$ — exercises
    # the full winbind auth path end to end (Kerberos/NTLM against the DC
    # we just provisioned, winbind name resolution on member, smbd's own
    # auth stack) without needing a share (IPC$ suffices — this harness,
    # like ad-member's, publishes none).
    member.wait_for_open_port(445)
    dcbox.succeed(
        "smbclient '//192.168.1.2/IPC$' -U 'NASTYDC\\alice%UserPass.123' -c 'exit'"
    )
  '';
}
