# Updating NASty

NASty updates are normally applied from the WebUI (Settings → Update). This page covers manual recovery when an automatic update fails.

## Flake layout change (mid-2026)

The NixOS flake was moved from `nixos/flake.nix` to the repository root (`flake.nix`). Instances installed before this change may fail to update automatically because the running engine references the old path.

### Symptoms

The update fails with errors like:

```
error: path '/etc/nixos/nixos/flake.nix' does not exist
```

or:

```
error: getting status of '/etc/nixos/nixos/flake.lock': No such file or directory
```

### Fix

SSH into your NASty box and run:

```bash
cd /etc/nixos
git fetch origin
git reset --hard origin/main
nixos-rebuild switch --flake /etc/nixos#nasty
```

After this rebuild the engine has the correct paths and future updates from the WebUI will work normally.

### Re-applying custom bcachefs version

If you had a custom bcachefs-tools version pinned, re-apply it after the manual update:

```bash
# Check what was pinned
cat /var/lib/nasty/bcachefs-tools-ref

# Re-apply (replace REF with the value from above)
cd /etc/nixos
nix flake lock --override-input bcachefs-tools "github:koverstreet/bcachefs-tools/REF"
nixos-rebuild switch --flake /etc/nixos#nasty
```
