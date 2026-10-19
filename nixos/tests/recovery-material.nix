# Phase-one qualification only: recover backup material without importing it.
# These are minimal VMs, not a claim of full NASty appliance recovery.
{ pkgs }:

let
  coreSources = [
    "/var/lib/nasty"
    "/etc/nixos"
    "/var/lib/caddy"
    "/var/lib/systemd/credential.secret"
    "/etc/machine-id"
  ];
  verifyCredential = pkgs.writeShellScript "verify-staged-recovery-credential" ''
    set -euo pipefail
    # Invoked in a private mount namespace: never replace the fresh host key.
    mount --make-rprivate /
    mount --bind /var/lib/recovery-test/staged/var/lib/systemd/credential.secret \
      /var/lib/systemd/credential.secret
    case "$1" in
      host-key-only) ;;
      source-identity)
        mount --bind /var/lib/recovery-test/staged/etc/machine-id /etc/machine-id
        ;;
      *) exit 2 ;;
    esac
    systemd-creds decrypt --name=nasty.recovery.fixture \
      /var/lib/recovery-test/staged/var/lib/nasty/fixture.cred /run/recovered-fixture
    printf synthetic-credential | cmp - /run/recovered-fixture
    rm /run/recovered-fixture
  '';
in pkgs.testers.runNixOSTest {
  name = "recovery-material";
  nodes = {
    original = { ... }: {
      environment.systemPackages = [ pkgs.rustic pkgs.openssl ];
    };
    replacement = { ... }: {
      environment.systemPackages = [ pkgs.rustic pkgs.openssl pkgs.util-linux pkgs.diffutils
        (import ../packages/recovery.nix { inherit pkgs; }) ];
    };
  };

  testScript = ''
    import json
    import pathlib
    import tempfile

    sources = json.loads('${builtins.toJSON coreSources}')
    original.start()
    original.wait_for_unit("multi-user.target")
    original.succeed("install -d -m 700 /var/lib/nasty /etc/nixos /var/lib/caddy /root/.ssh /run/repository")
    original.succeed("printf old-settings > /var/lib/nasty/settings.fixture")
    original.succeed("printf old-network > /var/lib/nasty/networking.json")
    original.succeed("printf old-boot-disk > /etc/nixos/hardware-configuration.nix")
    original.succeed("printf excluded-ssh-key > /root/.ssh/authorized_keys")
    original.succeed("systemd-creds setup")
    original.succeed("printf synthetic-credential | systemd-creds encrypt --with-key=host --name=nasty.recovery.fixture - /var/lib/nasty/fixture.cred")
    original.succeed("openssl req -x509 -newkey rsa:2048 -noenc -days 1 -subj /CN=recovery-fixture -keyout /var/lib/caddy/key.pem -out /var/lib/caddy/cert.pem")
    material_paths = [
      "/var/lib/nasty/settings.fixture", "/var/lib/nasty/networking.json",
      "/etc/nixos/hardware-configuration.nix", "/var/lib/nasty/fixture.cred",
      "/var/lib/caddy/key.pem", "/var/lib/caddy/cert.pem",
      "/var/lib/systemd/credential.secret",
      "/etc/machine-id",
    ]
    material_hashes = {path: original.succeed("sha256sum " + path).split()[0] for path in material_paths}
    original.succeed("printf independent-fixture-password > /run/repository-password")
    original.succeed("chmod 600 /run/repository-password")
    rustic = "rustic --repository /run/repository --password-file /run/repository-password"
    original.succeed(rustic + " init")
    original.succeed(rustic + " backup " + " ".join(sources))
    snapshot_id = original.succeed("find /run/repository/snapshots -type f -printf '%f\\n'").strip()
    assert len(snapshot_id) == 64, snapshot_id
    original.succeed("tar -C /run/repository -czf /run/repository.tar.gz .")

    # Only the encrypted repository survives. The replacement cannot read the
    # original VM's boot disk or use its service state to find credentials.
    with tempfile.TemporaryDirectory() as transfer:
      original.copy_from_machine("/run/repository.tar.gz", transfer)
      original.shutdown()
      replacement.start()
      replacement.wait_for_unit("multi-user.target")
      replacement.succeed("install -d -m 700 /run/repository /var/lib/recovery-test /var/lib/nasty /etc/nixos")
      replacement.succeed("printf fresh-network > /var/lib/nasty/networking.json")
      replacement.succeed("printf fresh-boot-disk > /etc/nixos/hardware-configuration.nix")
      replacement.succeed("systemd-creds setup")
      fresh_key_hash = replacement.succeed("sha256sum /var/lib/systemd/credential.secret")
      fresh_machine_id = replacement.succeed("cat /etc/machine-id")
      routes_before = replacement.succeed("ip -j route show")
      replacement.copy_from_host(str(pathlib.Path(transfer) / "repository.tar.gz"), "/run/repository.tar.gz")
      replacement.succeed("tar -C /run/repository -xzf /run/repository.tar.gz")

    with subtest("repository password is required independently of the lost installation"):
      replacement.succeed("printf wrong-password > /run/repository-password")
      replacement.fail(rustic + " restore latest /var/lib/recovery-test/wrong-password")
      replacement.succeed("test ! -e /var/lib/recovery-test/wrong-password/var/lib/nasty/settings.fixture")

    with subtest("recover into private staging without writing live configuration"):
      replacement.succeed("printf independent-fixture-password > /run/repository-password")
      replacement.succeed("chmod 600 /run/repository-password")
      replacement.succeed(rustic + " restore latest /var/lib/recovery-test/staged")
      for path, expected in material_hashes.items():
        assert replacement.succeed("sha256sum /var/lib/recovery-test/staged" + path).split()[0] == expected
      replacement.succeed("test \"$(cat /var/lib/recovery-test/staged/var/lib/nasty/settings.fixture)\" = old-settings")
      replacement.succeed("test \"$(cat /var/lib/recovery-test/staged/etc/nixos/hardware-configuration.nix)\" = old-boot-disk")
      replacement.succeed("openssl verify -CAfile /var/lib/recovery-test/staged/var/lib/caddy/cert.pem /var/lib/recovery-test/staged/var/lib/caddy/cert.pem")
      replacement.succeed("test ! -e /var/lib/recovery-test/staged/root/.ssh/authorized_keys")

    with subtest("host-only credential needs the saved host key and source machine identity"):
      assert replacement.succeed("cat /var/lib/recovery-test/staged/etc/machine-id") != fresh_machine_id
      replacement.fail("systemd-creds decrypt --name=nasty.recovery.fixture /var/lib/recovery-test/staged/var/lib/nasty/fixture.cred /run/wrong-key-output")
      replacement.fail("unshare --mount -- ${verifyCredential} host-key-only")
      replacement.succeed("unshare --mount -- ${verifyCredential} source-identity")
      assert replacement.succeed("sha256sum /var/lib/systemd/credential.secret") == fresh_key_hash
      assert replacement.succeed("cat /etc/machine-id") == fresh_machine_id

    with subtest("staging leaves fresh management and boot configuration unchanged"):
      command = replacement.succeed("nasty-recovery stage --source /var/lib/recovery-test/staged")
      staged = json.loads(command)
      assert staged["status"] == "staged", staged
      inspected = json.loads(replacement.succeed("nasty-recovery inspect " + staged["stage_id"]))
      assert inspected["import_supported"] is False, inspected
      assert inspected["coverage"]["source_machine_identity"] == "present", inspected
      assert inspected["coverage"]["host_credential_key"] == "present", inspected
      replacement.succeed("test \"$(stat -c %a /var/lib/nasty-recovery)\" = 700")
      replacement.succeed("test \"$(stat -c %a /var/lib/nasty-recovery/" + staged["stage_id"] + "/tree/etc/machine-id)\" = 600")
      assert replacement.succeed("sha256sum /var/lib/systemd/credential.secret") == fresh_key_hash
      assert replacement.succeed("cat /etc/machine-id") == fresh_machine_id
      replacement.succeed("test \"$(cat /var/lib/nasty/networking.json)\" = fresh-network")
      replacement.succeed("test \"$(cat /etc/nixos/hardware-configuration.nix)\" = fresh-boot-disk")
      assert replacement.succeed("ip -j route show") == routes_before
      replacement.succeed("test ! -e /var/lib/nasty/settings.fixture")

    with subtest("retrieve selected encrypted snapshot without manual extraction or ambient profiles"):
      replacement.succeed("printf '%s' '{\"repository\":\"/run/repository\",\"password_file\":\"/run/repository-password\"}' > /run/recovery-options.json")
      replacement.succeed("chmod 600 /run/recovery-options.json")
      replacement.succeed("mkdir -p /etc/rustic")
      replacement.succeed("printf intentionally-invalid-profile > /etc/rustic/rustic.toml")
      repository_hashes = replacement.succeed("find /run/repository -type f -exec sha256sum {} + | sort")
      listing = "nasty-recovery snapshots --options-file /run/recovery-options.json"
      workspace_before = replacement.succeed("ls -A /var/lib/nasty-recovery")
      listed = json.loads(replacement.succeed("RUSTIC_USE_PROFILE=ambient-evil RUSTIC_PASSWORD=wrong " + listing))
      assert listed["status"] == "listed", listed
      assert [s["id"] for s in listed["snapshots"]] == [snapshot_id], listed
      assert listed["snapshots"][0]["missing_recovery_paths"] == [], listed
      # Listing adds no stage session and leaves no scratch directory behind.
      assert replacement.succeed("ls -A /var/lib/nasty-recovery") == workspace_before
      retrieve = "nasty-recovery retrieve --options-file /run/recovery-options.json --snapshot " + listed["snapshots"][0]["id"]
      replacement.succeed("printf wrong-password > /run/repository-password")
      code, output = replacement.execute(listing)
      assert code != 0 and json.loads(output) == {"error": "repository_read_failed"}, output
      code, output = replacement.execute(retrieve)
      assert code != 0, output
      refused = json.loads(output)
      assert refused["status"] == "incomplete", refused
      assert "wrong-password" not in output, output
      replacement.fail("nasty-recovery inspect " + refused["stage_id"])
      replacement.succeed("printf independent-fixture-password > /run/repository-password")
      retrieved = json.loads(replacement.succeed("RUSTIC_USE_PROFILE=ambient-evil RUSTIC_PASSWORD=wrong " + retrieve))
      assert retrieved["status"] == "staged", retrieved
      retrieved_tree = "/var/lib/nasty-recovery/" + retrieved["stage_id"] + "/tree"
      for path, expected in material_hashes.items():
        assert replacement.succeed("sha256sum " + retrieved_tree + path).split()[0] == expected
      inspected = json.loads(replacement.succeed("nasty-recovery inspect " + retrieved["stage_id"]))
      assert inspected["retrieval"] == "authenticated_repository", inspected
      assert inspected["import_supported"] is False, inspected
      assert replacement.succeed("find /run/repository -type f -exec sha256sum {} + | sort") == repository_hashes
      replacement.succeed("test \"$(stat -c %a " + retrieved_tree + "/var/lib/caddy/key.pem)\" = 600")
      # Corrupt encrypted snapshot metadata: authentication failure must never
      # publish a completed staging session, even with the right password.
      replacement.succeed("printf corrupt > /run/repository/snapshots/" + snapshot_id)
      code, output = replacement.execute(retrieve)
      assert code != 0, output
      replacement.fail("nasty-recovery inspect " + json.loads(output)["stage_id"])
      assert replacement.succeed("sha256sum /var/lib/systemd/credential.secret") == fresh_key_hash
      assert replacement.succeed("cat /etc/machine-id") == fresh_machine_id
      assert replacement.succeed("ip -j route show") == routes_before
      replacement.succeed("test \"$(cat /var/lib/nasty/networking.json)\" = fresh-network")
      replacement.succeed("test \"$(cat /etc/nixos/hardware-configuration.nix)\" = fresh-boot-disk")
      replacement.succeed("test ! -e /var/lib/nasty/settings.fixture")

    with subtest("unsafe and incomplete CLI staging is refused without leaking paths"):
      replacement.fail("runuser -u nobody -- nasty-recovery stage --source /var/lib/recovery-test/staged")
      replacement.succeed("install -d -m 700 /var/lib/recovery-test/unsafe")
      replacement.succeed("ln -s /etc/shadow /var/lib/recovery-test/unsafe/sensitive-filename")
      code, output = replacement.execute("nasty-recovery stage --source /var/lib/recovery-test/unsafe")
      assert code != 0, output
      refusal = json.loads(output)
      assert refusal["error"] == "unsafe_entry", refusal
      assert "sensitive-filename" not in output, output
      replacement.fail("nasty-recovery inspect " + refusal["stage_id"])
      replacement.succeed("test -L /var/lib/recovery-test/unsafe/sensitive-filename")
      replacement.succeed("install -d -m 700 /var/lib/recovery-test/mounted/child")
      replacement.succeed("mount -t tmpfs tmpfs /var/lib/recovery-test/mounted/child")
      code, output = replacement.execute("nasty-recovery stage --source /var/lib/recovery-test/mounted")
      assert code != 0, output
      assert json.loads(output)["error"] == "mount_boundary", output
      replacement.succeed("umount /var/lib/recovery-test/mounted/child")
  '';
}
