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
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Record the exact independently-pinned bcachefs/kernel/tool environment.
    machine.succeed("uname -a")
    machine.succeed("bcachefs version")
    machine.succeed("setproject --version || true")

    machine.succeed("bcachefs format --force /dev/vdb")
    machine.succeed("mkdir /mnt/bcachefs && mount -t bcachefs /dev/vdb /mnt/bcachefs")
    machine.succeed("printf 'source:1001\\nclone:1002\\nsnapshot-clone:1003\\n' > /etc/projid")

    # Baseline: a writable clone accepts a distinct project ID.
    machine.succeed("bcachefs subvolume create /mnt/bcachefs/source")
    machine.succeed("printf fixture > /mnt/bcachefs/source/data")
    machine.succeed("setproject -c -P source /mnt/bcachefs/source")
    machine.succeed("bcachefs subvolume snapshot /mnt/bcachefs/source /mnt/bcachefs/clone")
    machine.succeed("setproject -c -P clone /mnt/bcachefs/clone")

    # Candidate reproducer: create a read-only snapshot, materialize it as a
    # writable subvolume, then assign its own project ID. The appliance crash
    # occurred on this last operation after an engine restart.
    machine.succeed("bcachefs subvolume snapshot -r /mnt/bcachefs/source /mnt/bcachefs/point")
    machine.succeed("bcachefs subvolume snapshot /mnt/bcachefs/point /mnt/bcachefs/snapshot-clone")
    machine.succeed("setproject -c -P snapshot-clone /mnt/bcachefs/snapshot-clone")
    machine.succeed("test \"$(cat /mnt/bcachefs/snapshot-clone/data)\" = fixture")
  '';
}
