# Minimal bcachefs project-ID reproducer. It deliberately contains no NASty
# services, API calls, crash injection, networking, or storage recovery code.
{ pkgs, nasty-bcachefs-tools }:

pkgs.testers.runNixOSTest {
  name = "bcachefs-project-quota-repro";

  nodes.machine = {
    imports = [ ../modules/bcachefs.nix ../modules/linuxquota.nix ];
    _module.args = { inherit nasty-bcachefs-tools; };

    virtualisation.emptyDiskImages = [ 1024 ];
    virtualisation.memorySize = 1024;
  };

  testScript = ''
    machine.start(allow_reboot=True)
    machine.wait_for_unit("multi-user.target")

    # Record the exact independently-pinned bcachefs/kernel/tool environment.
    machine.succeed("uname -a")
    machine.succeed("bcachefs version")
    machine.succeed("setproject --version || true")

    machine.succeed("bcachefs format --force /dev/vdb")
    machine.succeed("mkdir -p /mnt/bcachefs && mount -t bcachefs /dev/vdb /mnt/bcachefs")
    # These clone IDs are the exact values in the #980 kernel-BUG trace.
    # Keep them rather than small illustrative IDs to cover any width/sign or
    # allocation-bucket interaction in bcachefs quota handling.
    machine.succeed("printf 'source:1001\\nclone:2378439521\\nsnapshot-clone:2944444548\\n' > /etc/projid")

    # Build the same topology before assigning IDs to the clones. The source
    # is assigned first (as happens during source creation); clone IDs are
    # assigned only after the remount below.
    machine.succeed("bcachefs subvolume create /mnt/bcachefs/source")
    machine.succeed("printf fixture > /mnt/bcachefs/source/data")
    machine.succeed("setproject -c -P source /mnt/bcachefs/source")
    # Atomic provisioning creates clones below a private same-filesystem
    # staging directory, then atomically publishes them at the pool root.
    # Reproduce that layout without invoking any appliance code.
    machine.succeed("mkdir -p /mnt/bcachefs/.stage/clone /mnt/bcachefs/.stage/snapshot-clone")
    machine.succeed("bcachefs subvolume snapshot /mnt/bcachefs/source /mnt/bcachefs/.stage/clone/volume")
    machine.succeed("mv /mnt/bcachefs/.stage/clone/volume /mnt/bcachefs/clone")
    machine.succeed("bcachefs subvolume snapshot -r /mnt/bcachefs/source /mnt/bcachefs/point")
    machine.succeed("bcachefs subvolume snapshot /mnt/bcachefs/point /mnt/bcachefs/.stage/snapshot-clone/volume")
    machine.succeed("mv /mnt/bcachefs/.stage/snapshot-clone/volume /mnt/bcachefs/snapshot-clone")

    # #980's fault was after an engine-process restart with the pool mounted.
    # A full VM reboot/remount is stronger at the filesystem boundary and does
    # not introduce appliance code; it establishes whether persisted quota
    # state is sufficient to trigger it.
    machine.succeed("sync")
    machine.reboot()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("mkdir -p /mnt/bcachefs && mount -t bcachefs /dev/vdb /mnt/bcachefs")

    # Control followed by the candidate operation. The appliance fault was on
    # the final assignment after the writable clone assignment had succeeded.
    machine.succeed("setproject -c -P clone /mnt/bcachefs/clone")
    machine.succeed("setproject -c -P snapshot-clone /mnt/bcachefs/snapshot-clone")
    machine.succeed("test \"$(cat /mnt/bcachefs/snapshot-clone/data)\" = fixture")
  '';
}
