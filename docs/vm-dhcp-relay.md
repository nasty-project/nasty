# Routed VM bridge with DHCP relay

Use a separate routed subnet when DHCP-based VM appliances need LAN-accessible
services without sharing the host or Kubernetes/MetalLB address pool.

1. In **Settings → Network**, or the VM bridge creator, create a bridge with
   **no members**, one static IPv4 address such as `10.10.30.1/24`, and no host
   default gateway. The existing management interface and default route stay
   unchanged.
2. Enable **Relay guest DHCP to an upstream server**, enter the DHCP server's
   IPv4 address, and choose its upstream interface. Existing bridges expose the
   same controls in their interface settings.
3. Configure the upstream router/DHCP server:
   - Route `10.10.30.0/24` through NASty's management address.
   - Create a DHCP scope for that subnet, with guest gateway `10.10.30.1`, the
     desired DNS server, and a pool that excludes the bridge address.
   - Accept relayed DHCP requests with relay address `10.10.30.1`.
   - For Internet access, include the guest subnet in the router's WAN NAT
     policy, or provide upstream routing for it.
4. Attach the VM NIC to the bridge and use DHCP inside the guest. Access its
   services using the assigned guest address, not a QEMU `10.0.2.x` address.

NASty runs a dedicated DNS-disabled dnsmasq relay. It does **not** allocate
leases, configure your router, add host NAT, or enforce security isolation
between the routed subnet and your LAN. IPv4 forwarding is enabled on the
appliance. Use appropriate router/firewall policies if isolation is required.

The bridge's DHCP relay settings live in `networking.json` and survive reboot.
They participate in network confirmation/rollback. Generated UDP 67 input rules
accept client requests only on the chosen bridge, and server replies only from
the configured server on the upstream interface. Firewall service changes retain
these rules. Disabling the relay removes its rules and stops the daemon.

Validation rejects physical bridge members, dynamic/multiple bridge addresses,
host default gateways, overlapping configured subnets, and invalid server or
interface values. Before starting the daemon, NASty checks that the bridge has
its configured address and that the server routes through the selected upstream.
DHCPv6, local DHCP pools, and automatically configuring upstream routers are not
supported by this feature.

For troubleshooting:

```sh
systemctl status nasty-dhcp-relay.service
journalctl -u nasty-dhcp-relay.service
ip -4 addr show br-vm
ip -4 route get <dhcp-server-address>
nft list chain inet nasty dhcp_relay
```

Check the upstream route and DHCP scope if requests are forwarded but no leases
arrive. Relay startup failures appear in the network update response; partial
network application is not reported as a successful bridge creation.
