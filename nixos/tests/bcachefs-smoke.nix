# Boots a minimal NixOS VM with our bcachefs DKMS module, formats a
# virtual disk, mounts it, and prints the raw output of `bcachefs fs usage`
# and `bcachefs show-super` so the test log captures real fixtures we can
# paste into nasty-storage parser tests.

{ pkgs, nasty-bcachefs-tools }:

pkgs.testers.runNixOSTest {
  name = "bcachefs-smoke";

  nodes.machine = {
    imports = [ ../modules/bcachefs.nix ];
    _module.args = { inherit nasty-bcachefs-tools; };

    virtualisation.emptyDiskImages = [ 1024 ];
    virtualisation.memorySize = 1024;
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # Format the empty 1 GiB disk and mount it.
    machine.succeed("bcachefs format /dev/vdb")
    machine.succeed("mkdir -p /mnt/test")
    machine.succeed("mount -t bcachefs /dev/vdb /mnt/test")

    # Capture raw outputs to the test log. These are the fixtures
    # nasty-storage parsers (parse_device_table_line, parse_human_bytes,
    # parse_bcachefs_opt) need to be tested against.
    fs_usage = machine.succeed("bcachefs fs usage /mnt/test")
    show_super = machine.succeed("bcachefs show-super /dev/vdb")

    print("=== bcachefs fs usage /mnt/test ===")
    print(fs_usage)
    print("=== bcachefs show-super /dev/vdb ===")
    print(show_super)
  '';
}
