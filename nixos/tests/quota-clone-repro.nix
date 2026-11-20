# Throwaway reproducer for the bcachefs quota-transfer BUG seen in PR #980
# (kernel BUG at fs/fs/quota.c:325, __bch2_quota_transfer, via setproject).
#
# Hypothesis: a snapshot clone inherits its source's project ID, but its usage
# is not charged to that project until the next mount. Moving one clone to its
# own project debits the source project once; moving a second clone of the
# same source before a remount debits it again and underflows the counter.
#
# Each case gets a fresh filesystem on its own disk. Cases expected to be safe
# run first; the suspected crashers run last, since a BUG can wedge the VM.
# The test reports outcomes rather than asserting them.

{ pkgs, nasty-bcachefs-tools }:

pkgs.testers.runNixOSTest {
  name = "quota-clone-repro";

  nodes.machine = {
    imports = [ ../modules/bcachefs.nix ../modules/linuxquota.nix ];
    _module.args = { inherit nasty-bcachefs-tools; };

    virtualisation.emptyDiskImages = [ 1024 1024 1024 1024 ];
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    print(machine.succeed("uname -r; bcachefs version; command -v setproject repquota"))

    for i, name in enumerate(["src", "c1", "c2", "c3"], start=1000):
        machine.succeed(f"echo 'p-{name}:{i}' >> /etc/projid")

    def kernel_bug():
        return machine.execute("dmesg | grep -E 'kernel BUG|__bch2_quota_transfer'")[0] == 0

    def report(mnt, label):
        print(f"--- {label}: repquota -P {mnt}")
        print(machine.execute(f"repquota -P -n {mnt} 2>&1")[1])

    def setproject(path, project):
        try:
            status, out = machine.execute(
                f"timeout -s KILL 30 setproject -c -P {project} {path} 2>&1; echo exit=$?",
                timeout=60,
            )
            return out.strip()
        except Exception as e:  # stuck in the kernel
            return f"hung: {e}"

    def prepare(disk, mnt):
        machine.succeed(f"bcachefs format -q {disk}")
        machine.succeed(f"mkdir -p {mnt} && mount -t bcachefs -o prjquota {disk} {mnt}")
        machine.succeed(f"bcachefs subvolume create {mnt}/src")
        machine.succeed(f"setproject -c -P p-src {mnt}/src")
        machine.succeed(f"dd if=/dev/urandom of={mnt}/src/data bs=1M count=100 status=none && sync")

    def remount(disk, mnt):
        machine.succeed(f"umount {mnt} && mount -t bcachefs -o prjquota {disk} {mnt}")

    results = {}

    def case(name, disk, steps):
        if kernel_bug():
            results[name] = "skipped: kernel already hit the BUG"
            return
        mnt = f"/mnt/{name}"
        prepare(disk, mnt)
        report(mnt, f"{name} after source write")
        outcome = []
        for step in steps:
            kind, *args = step
            if kind == "clone":
                machine.succeed(f"bcachefs subvolume snapshot {mnt}/src {mnt}/{args[0]}")
            elif kind == "snapclone":
                machine.succeed(f"bcachefs subvolume snapshot -r {mnt}/src {mnt}/{args[0]}-ro")
                machine.succeed(f"bcachefs subvolume snapshot {mnt}/{args[0]}-ro {mnt}/{args[0]}")
            elif kind == "remount":
                remount(disk, mnt)
                report(mnt, f"{name} after remount")
            elif kind == "setproject":
                res = setproject(f"{mnt}/{args[0]}", f"p-{args[0]}")
                outcome.append(f"setproject {args[0]}: {res}")
                if kernel_bug():
                    outcome.append("KERNEL BUG")
                    break
                report(mnt, f"{name} after setproject {args[0]}")
        results[name] = "; ".join(outcome)

    # Expected safe.
    case("single-clone", "/dev/vdb", [("clone", "c1"), ("setproject", "c1")])
    case("two-clones-remount", "/dev/vdc",
         [("clone", "c1"), ("clone", "c2"), ("setproject", "c1"), ("remount",), ("setproject", "c2")])
    # Suspected crashers.
    case("two-clones", "/dev/vdd", [("clone", "c1"), ("clone", "c2"), ("setproject", "c1"), ("setproject", "c2")])
    case("clone-and-snapclone", "/dev/vde",
         [("clone", "c1"), ("snapclone", "c3"), ("setproject", "c1"), ("setproject", "c3")])

    print("=== quota-clone-repro results ===")
    for name, outcome in results.items():
        print(f"{name}: {outcome}")
    print("=== dmesg (bcachefs / BUG) ===")
    print(machine.execute("dmesg | grep -iE 'bcachefs|BUG|quota' | tail -80")[1])
  '';
}
