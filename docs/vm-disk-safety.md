# VM disk attachment safety

The stopped-VM Storage tab offers **Create new disk** (filesystem, name, size
in GiB) and **Create and attach**. This creates a dedicated block subvolume
under `vms/` using `vm.disk.create`, then attaches it with `vm.update`. If
attachment fails, the volume is kept; find it under Subvolumes and retry
attachment instead of creating another disk. No existing volume is deleted.

Existing-disk choices exclude known consumers. The tab explains unavailable
volumes with their configured VM name (including stopped VMs), iSCSI target,
NVMe-oF subsystem, Kubernetes CSI/PVC metadata, or local whole-device mount.
CSI metadata reserves a volume even while its PVC is not mounted.

`vm.disk.candidates` returns visible block subvolumes and consumer descriptions.
An inventory error disables existing-disk selection. VM create/update and
explicit start also check conflicts server-side, under the shared block
mutation lock; protocol exports cannot claim disks reserved by stopped VMs.
Saved backing-file paths take precedence over recycled loop-device numbers.

**No known consumer is not proof that a disk is empty or safe.** Manual
processes, partition mounts, external ownership without CSI metadata, and
changes made outside NASty cannot all be detected. Existing-disk attachment
requires an overwrite warning confirmation. Only attach volumes whose contents
and ownership you understand. These checks do not inspect or repair data and
do not remove previously configured attachments. Boot-time VM autostart is
not routed through the explicit-start RPC check.

## Regression validation

- Rust router tests cover CSI/PVC metadata, backing-file identity, aliases,
  recycled loop numbers, and local mount decoding.
- Frontend tests cover candidate filtering and inline creation input limits.
- The Linux appliance smoke test covers stopped-VM reservations, device aliases,
  rejected create/update without changing the recipient, inactive CSI ownership,
  fresh volume creation/attachment, and corrupt VM inventory failing closed.
