# This file is overwritten by the installer with the correct disk path.
# The default "nodev" means GRUB won't install to MBR (UEFI-only fallback).
{ ... }: { boot.loader.grub.device = "nodev"; }
