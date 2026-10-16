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
      environment.systemPackages = [ pkgs.rustic pkgs.openssl pkgs.util-linux pkgs.diffutils ];
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
      replacement.succeed("test \"$(cat /var/lib/nasty/networking.json)\" = fresh-network")
      replacement.succeed("test \"$(cat /etc/nixos/hardware-configuration.nix)\" = fresh-boot-disk")
      assert replacement.succeed("ip -j route show") == routes_before
      replacement.succeed("test ! -e /var/lib/nasty/settings.fixture")
  '';
}
