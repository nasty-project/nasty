# NASty Testing Guide

## Option 1: QEMU VM (recommended for first test)

Build and run a VM with 3 virtual 1GB disks:

```bash
cd nixos

# Build the VM (first run downloads NixOS, takes a while)
nix build .#nixosConfigurations.nasty-vm.config.system.build.vm

# Run it
./result/bin/run-nasty-vm
```

Access from your host:
- **WebUI**: https://localhost:8443 (accept self-signed cert)
- **SSH**: `ssh -p 2222 root@localhost` (password: `nasty`)
- **Health**: `curl -k https://localhost:8443/health`

Default WebUI login: `admin` / `admin`

### VM test checklist

```bash
# SSH into the VM
ssh -p 2222 root@localhost

# 1. Check middleware is running
systemctl status nasty-middleware
journalctl -u nasty-middleware --no-pager -n 20

# 2. Check nginx/TLS is working
curl -k https://localhost/health

# 3. Check self-signed cert was generated
ls -la /var/lib/nasty/tls/

# 4. List available virtual disks
lsblk

# 5. Test pool creation (use virtual disks)
# Do this from the WebUI, or manually:
bcachefs format /dev/vdb
mkdir -p /mnt/nasty/testpool
bcachefs mount /dev/vdb /mnt/nasty/testpool

# 6. Test subvolume creation
bcachefs subvolume create /mnt/nasty/testpool/mydata

# 7. Check NFS server
systemctl status nfs-server
cat /etc/exports.d/nasty.exports 2>/dev/null || echo "(empty - no shares yet)"

# 8. Check Samba
systemctl status smb

# 9. Test all WebUI pages in your browser
```

## Option 2: Build ISO for bare metal

```bash
cd nixos

# Build the installer ISO
nix build .#nixosConfigurations.nasty-iso.config.system.build.isoImage

# ISO will be at:
ls result/iso/
```

Flash to USB:
```bash
sudo dd if=result/iso/nixos-*.iso of=/dev/sdX bs=4M status=progress
```

Boot from USB and run `nasty-install` for guided setup.

## Option 3: Deploy to existing NixOS

Add NASty as a flake input in your system configuration:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nasty.url = "path:/path/to/NAS/nixos"; # or github:your-org/nasty
  };

  outputs = { nixpkgs, nasty, ... }: {
    nixosConfigurations.mynas = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        nasty.nixosModules.nasty
        ./hardware-configuration.nix
        {
          services.nasty = {
            enable = true;
            nfs.enable = true;
            smb.enable = true;
            iscsi.enable = true;
            nvmeof.enable = true;

            # Optional: use your own TLS cert
            # tls.certFile = "/etc/ssl/nasty/cert.pem";
            # tls.keyFile = "/etc/ssl/nasty/key.pem";
          };
        }
      ];
    };
  };
}
```

Then: `sudo nixos-rebuild switch --flake .#mynas`

## Troubleshooting

### Middleware won't start
```bash
journalctl -u nasty-middleware -f
# Check state directory exists
ls -la /var/lib/nasty/
```

### WebUI shows "Connecting to middleware..."
```bash
# Check middleware is listening
ss -tlnp | grep 2137
# Check nginx is proxying
journalctl -u nginx -f
```

### Self-signed cert issues
```bash
# Regenerate cert
rm /var/lib/nasty/tls/*.pem
systemctl restart nasty-selfsigned-cert
systemctl restart nginx
```

### Pool operations fail
```bash
# Ensure bcachefs-tools is available
which bcachefs
# Check kernel support
cat /proc/filesystems | grep bcachefs
```

## Pre-build verification (without NixOS)

You can verify the components build on any system:

```bash
# Middleware (requires Rust toolchain)
cd middleware && cargo build

# WebUI (requires Node.js)
cd webui && npm install && npm run build

# Type checking
cd webui && npx svelte-check
```
