# Storage maintenance mode

Maintenance mode is for checking or repairing **data pools**, not the running OS
filesystem. The OS partition remains mounted and may share a physical disk with
a data pool. Never run repairs against the OS partition or the whole disk in
place of the pool's actual member devices.

## Enter

First verify you can log in over SSH with a local OS account or use the local
console. Your existing SSH keys and authentication policy remain in effect;
domain logins may depend on Samba services that will be disabled. The WebUI
does not remain available during maintenance.

As a root-equivalent administrator, select **Power → Storage maintenance** and
confirm, or run:

```sh
sudo nasty-maintenance enter
```

The command saves a persistent flag on the OS filesystem before scheduling a
reboot. **Pools may still be mounted until that reboot completes.** If writing
the flag fails, no reboot is scheduled. If reboot scheduling fails, the flag
remains set; check `nasty-maintenance status` and reboot manually when ready.

## During maintenance

```sh
sudo nasty-maintenance status
findmnt -t bcachefs
lsblk -f
```

The engine, metrics, WebUI, managed shares, Docker (including socket activation
and pruning), storage exports, and watchdog do not start automatically. The
engine therefore cannot restore pools, apps, VMs, backups, or scheduled storage
jobs. Declarative mounts/automounts below `/fs` are also guarded. The root/OS
filesystem and networking remain available; the maintenance firewall permits
the configured SSH ports. Previously enabled Tailscale is started separately
using its existing daemon state. It still needs valid tailnet authorization and
reachable networking; retain a LAN/console recovery path.

Verify the relevant pools are actually unmounted and identify **all correct
member devices** before running any offline filesystem check or repair. No
checks, repairs, forced unmounts, or filesystem modifications run automatically.
Custom services/mounts outside NASty's managed storage paths are not covered;
stop and unmount those yourself. Do not manually mount a pool or start consumers
while running an offline check. Maintenance guards do not prevent root from
running arbitrary commands.

Maintenance survives further reboots. Both the durable flag
`/var/lib/nasty/maintenance` and this boot's latch `/run/nasty-maintenance` block
consumer startup. Do not remove these by hand.

## Exit

When all checks/repairs have completed and no offline maintenance job is running:

```sh
sudo nasty-maintenance exit
```

This removes the durable flag and schedules a normal reboot. The current boot
stays in maintenance until reboot, including after `systemctl daemon-reload`.
Normal startup then restores the saved pool and consumer configuration. An
unrelated older system generation that predates this feature does not implement
these guards; do not boot it expecting maintenance protection.

`enter --no-reboot` and `exit --no-reboot` only schedule the next boot's mode;
they are useful when a reboot must be coordinated separately. They never claim
to make the current running pools offline.
