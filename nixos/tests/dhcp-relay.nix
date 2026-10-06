# A real relay/server/client exchange, routed guest HTTP access, simulated WAN
# access through router NAT, and confirmed/rolled-back reboot persistence.
{ pkgs, nasty-engine, nasty-webui, nasty-bcachefs-tools }:
let
  python = pkgs.python3.withPackages (p: [ p.websocket-client ]);
  rpc = pkgs.writeText "relay-rpc.py" ''
    import json
    import pathlib
    import sys
    import urllib.request
    import websocket

    password_file = pathlib.Path("/root/relay-test-password")
    password = password_file.read_text() if password_file.exists() else "admin"
    def connect(password):
        req = urllib.request.Request("http://127.0.0.1:2137/api/login",
            data=json.dumps({"username":"admin", "password":password}).encode(),
            headers={"Content-Type":"application/json"})
        token = json.loads(urllib.request.urlopen(req).read())["token"]
        ws = websocket.create_connection("ws://127.0.0.1:2137/ws", timeout=60)
        ws.send(json.dumps({"token":token}))
        auth = json.loads(ws.recv())
        assert auth["authenticated"], auth
        return ws, auth
    ws, auth = connect(password)
    serial = 0
    def call(method, params=None):
        global serial
        serial += 1
        ws.send(json.dumps({"jsonrpc":"2.0", "id":serial, "method":method, "params":params}))
        while True:
            response = json.loads(ws.recv())
            if response.get("id") == serial:
                assert "error" not in response, response
                return response["result"]
    if auth.get("must_change_password"):
        password = "relay-Test-Password-123!"
        call("auth.change_password", {"username":"admin", "new_password":password})
        password_file.write_text(password)
        ws.close()
        ws, auth = connect(password)

    config = call("system.network.get")["config"]
    mode = sys.argv[1]
    if mode == "enable":
        config["interfaces"] = [i for i in config["interfaces"] if i["name"] != "eth1"] + [{
            "name":"eth1", "enabled":True,
            "ipv4":{"method":"static", "addresses":["10.10.20.100/28"], "gateway":None},
            "ipv6":{"method":"disabled", "addresses":[], "gateway":None}}]
        config["bridges"] = [{"name":"br-vm", "members":[],
            "ipv4":{"method":"static", "addresses":["10.10.30.1/24"], "gateway":None},
            "ipv6":{"method":"disabled", "addresses":[], "gateway":None},
            "dhcp_relay":{"server":"10.10.20.97", "upstream":"eth1"}}]
    elif mode in ("disable", "rollback", "pending"):
        config["bridges"][0]["dhcp_relay"] = None
    else:
        raise AssertionError(mode)
    config["confirm_within_secs"] = 5 if mode == "rollback" else 120
    response = call("system.network.update", config)
    assert not response.get("apply_errors"), response
    if mode not in ("rollback", "pending") and response.get("txn_id"):
        call("system.network.confirm", {"txn_id":response["txn_id"]})
    ws.close()
  '';
  leaseScript = pkgs.writeShellScript "relay-lease" ''
    if [ "$1" = bound ] || [ "$1" = renew ]; then
      ip addr flush dev "$interface"
      ip addr add "$ip/24" dev "$interface"
      ip route replace default via "$router" dev "$interface"
      printf '%s\n' "$ip" "$router" "$dns" > /run/relay-lease
    fi
  '';
in pkgs.testers.runNixOSTest {
  name = "dhcp-relay";
  nodes.machine = { lib, ... }: {
    imports = [ ../modules/bcachefs.nix ../modules/linuxquota.nix ../modules/nasty.nix ];
    _module.args = { inherit nasty-engine nasty-webui nasty-bcachefs-tools; nasty-version = "test"; };
    services.nasty = {
      enable = true;
      engine.package = nasty-engine;
      webui.package = nasty-webui;
      nfs.enable = false;
      smb.enable = false;
      iscsi.enable = false;
      nvmeof.enable = false;
    };
    services.timesyncd.enable = lib.mkForce false;
    environment.systemPackages = [ python pkgs.busybox pkgs.dnsutils ];
    virtualisation.memorySize = 2048;
    virtualisation.vlans = [ 1 ];
  };
  nodes.router = { ... }: {
    environment.systemPackages = [ pkgs.curl ];
    virtualisation.vlans = [ 1 2 ];
    networking.useDHCP = false;
    networking.interfaces.eth1.ipv4.addresses = [ { address = "10.10.20.97"; prefixLength = 28; } ];
    networking.interfaces.eth2.ipv4.addresses = [ { address = "203.0.113.1"; prefixLength = 24; } ];
    networking.interfaces.lo.ipv4.addresses = [ { address = "10.10.20.110"; prefixLength = 32; } ];
    networking.interfaces.eth1.ipv4.routes = [ { address = "10.10.30.0"; prefixLength = 24; via = "10.10.20.100"; } ];
    networking.firewall.allowedUDPPorts = [ 53 67 ];
    networking.nat = { enable = true; externalInterface = "eth2"; internalIPs = [ "10.10.0.0/16" ]; };
    services.dnsmasq = {
      enable = true;
      settings = {
        interface = "eth1";
        bind-interfaces = true;
        dhcp-range = "10.10.30.50,10.10.30.199,255.255.255.0,2h";
        dhcp-option = [ "option:router,10.10.30.1" "option:dns-server,10.10.20.97" ];
        dhcp-host = "02:00:00:00:30:50,10.10.30.50";
        host-record = "wan-test.example,203.0.113.2";
        log-dhcp = true;
      };
    };
  };
  nodes.outside = { ... }: {
    virtualisation.vlans = [ 2 ];
    networking.useDHCP = false;
    networking.interfaces.eth1.ipv4.addresses = [ { address = "203.0.113.2"; prefixLength = 24; } ];
    networking.firewall.allowedTCPPorts = [ 8080 ];
    systemd.services.wan-http = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 -m http.server 8080 --bind 203.0.113.2";
    };
  };
  testScript = ''
    machine.start(allow_reboot=True)
    router.start()
    outside.start()
    router.wait_for_unit("dnsmasq.service")
    outside.wait_for_unit("wan-http.service")
    machine.wait_for_unit("nasty-engine.service")
    machine.wait_until_succeeds("curl -fsS http://127.0.0.1:2137/health")
    before_default = machine.succeed("ip -4 route show default")
    machine.succeed("${python}/bin/python3 ${rpc} enable")
    machine.wait_for_unit("nasty-dhcp-relay.service")

    def guest():
        machine.succeed("ip netns add guest")
        machine.succeed("ip link add vm-tap type veth peer name guest-eth")
        machine.succeed("ip link set guest-eth netns guest")
        machine.succeed("ip link set vm-tap master br-vm")
        machine.succeed("ip link set vm-tap up")
        machine.succeed("ip netns exec guest ip link set guest-eth address 02:00:00:00:30:50")
        machine.succeed("ip netns exec guest ip link set guest-eth up")
        machine.succeed("ip netns exec guest ip link set lo up")
        machine.succeed("ip route replace 203.0.113.0/24 via 10.10.20.97 dev eth1")
        machine.succeed("ip netns exec guest ${pkgs.busybox}/bin/udhcpc -i guest-eth -s ${leaseScript} -n -q -t 10 -T 2", timeout=40)
        lease = machine.succeed("cat /run/relay-lease").splitlines()
        assert lease == ["10.10.30.50", "10.10.30.1", "10.10.20.97"], lease
        machine.succeed("ip netns exec guest ${pkgs.busybox}/bin/httpd -p 8081 -h /run")
        router.succeed("curl -fsS http://10.10.30.50:8081/relay-lease | grep -Fx 10.10.30.50")
        machine.succeed("ip netns exec guest dig +short @10.10.20.97 wan-test.example | grep -Fx 203.0.113.2")
        machine.succeed("ip netns exec guest curl -fsS http://203.0.113.2:8080/ > /dev/null")
        outside.succeed("journalctl -u wan-http.service | grep -F 203.0.113.1")
        # Existing management and a representative Kubernetes service /32
        # remain reachable; guest addresses never use that service range.
        router.succeed("curl -ksSf https://10.10.20.100/health")
        machine.succeed("ping -c 1 10.10.20.110")

    guest()
    assert machine.succeed("ip -4 route show default") == before_default
    # Replacing the whole firewall for a protocol change retains relay rules.
    machine.succeed("systemctl restart nasty-engine.service")
    machine.wait_for_unit("nasty-engine.service")
    machine.wait_for_unit("nasty-dhcp-relay.service")
    assert "10.10.20.97" in machine.succeed("nft list chain inet nasty dhcp_relay")
    machine.succeed("ip netns exec guest ${pkgs.busybox}/bin/udhcpc -i guest-eth -s ${leaseScript} -n -q -t 10 -T 2", timeout=40)

    # Timed rollback restores the daemon and generated firewall policy.
    machine.succeed("${python}/bin/python3 ${rpc} rollback")
    machine.fail("systemctl is-active --quiet nasty-dhcp-relay.service")
    machine.wait_until_succeeds("systemctl is-active --quiet nasty-dhcp-relay.service")
    assert "10.10.20.97" in machine.succeed("nft list chain inet nasty dhcp_relay")

    # Reboot with an unconfirmed disable: startup recovers the confirmed relay.
    machine.succeed("${python}/bin/python3 ${rpc} pending")
    machine.reboot()
    machine.wait_for_unit("nasty-engine.service")
    machine.wait_for_unit("nasty-dhcp-relay.service")
    guest()
    machine.succeed("${python}/bin/python3 ${rpc} disable")
    machine.fail("systemctl is-active --quiet nasty-dhcp-relay.service")
    rules = machine.succeed("nft list chain inet nasty dhcp_relay")
    assert "udp" not in rules, rules
    machine.reboot()
    machine.wait_for_unit("nasty-engine.service")
    machine.fail("systemctl is-active --quiet nasty-dhcp-relay.service")
    assert "udp" not in machine.succeed("nft list chain inet nasty dhcp_relay")
  '';
}
