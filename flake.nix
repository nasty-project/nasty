{
  description = "NASty - NAS System built on NixOS and bcachefs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # ── bcachefs override (optional) ──────────────────────────────
    # Pinned to v1.37 release tag.
    # To revert to pure nixpkgs: comment out these two lines.
    # No other changes needed — bcachefs.nix defaults to pkgs.bcachefs-tools.
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.5";
    bcachefs-tools.inputs.nixpkgs.follows = "nixpkgs";

  };

  outputs = { self, nixpkgs, bcachefs-tools, ... }: let
    # Helper to build packages for a given system
    mkPkgs = system: nixpkgs.legacyPackages.${system};
    nasty-version = (builtins.fromTOML (builtins.readFile ./engine/Cargo.toml)).workspace.package.version;
    rootLock = builtins.fromJSON (builtins.readFile ./flake.lock);
    installerNastyOwner = "nasty-project";
    installerNastyRepo = "nasty";

    mkEngine = system: let pkgs = mkPkgs system; in pkgs.rustPlatform.buildRustPackage {
      pname = "nasty-engine";
      version = nasty-version;
      src = ./engine;
      cargoLock.lockFile = ./engine/Cargo.lock;
      meta = {
        description = "NASty NAS engine";
        license = pkgs.lib.licenses.gpl3Only;
      };
    };

    mkWebui = system: let pkgs = mkPkgs system; in pkgs.buildNpmPackage {
      pname = "nasty-webui";
      version = nasty-version;
      src = ./webui;
      npmDepsHash = "sha256-WDmf2sCREKbMK8ec5zlA4lnrM8oyt0Ntbi6OWztYBMM=";
      npmFlags = [ "--legacy-peer-deps" ];
      buildPhase = ''
        npm run prepare
        npm run build
      '';
      installPhase = ''
        mkdir -p $out/share/nasty-webui
        cp -r build/* $out/share/nasty-webui/
      '';
    };

    mkBcachefsTools = system: let
      pkgs = mkPkgs system;
      # Override nixpkgs' bcachefs-tools with HEAD source from the flake input.
      # Using the nixpkgs package as the base preserves the `dkms` output and
      # `passthru.kernelModule` that the NixOS bcachefs module needs to build
      # the out-of-tree DKMS kernel module automatically via boot.bcachefs.package.
      # importCargoLock reads Cargo.lock directly — no pre-computed vendor hash needed.
      #
      # CONFIG_BCACHEFS_QUOTA: bcachefs is an out-of-tree DKMS module, so
      # its own Kconfig is never processed by the host kernel's build system.
      # We patch the DKMS Makefile to inject -DCONFIG_BCACHEFS_QUOTA directly,
      # enabling the VFS quotactl_ops (sb->s_qcop) that setquota/repquota need.
      base = pkgs.bcachefs-tools.overrideAttrs (old: {
        version = (builtins.fromTOML (builtins.readFile "${bcachefs-tools}/Cargo.toml")).package.version;
        src = bcachefs-tools;
        cargoDeps = pkgs.rustPlatform.importCargoLock {
          lockFile = "${bcachefs-tools}/Cargo.lock";
        };
      });
    in base.overrideAttrs (old: {
      passthru = old.passthru // {
        # kernelModule must keep the same named-attr signature that callPackage
        # expects: { lib, stdenv, kernelModuleMakeFlags, kernel } -> drv.
        kernelModule = { lib, stdenv, kernelModuleMakeFlags, kernel }:
          (old.passthru.kernelModule { inherit lib stdenv kernelModuleMakeFlags kernel; }).overrideAttrs (kOld: {
            postPatch = (kOld.postPatch or "") + ''
              # ccflags-y in the top-level Makefile only covers objects built
              # there.  The actual compilation happens in src/fs/bcachefs/,
              # so we patch that subdir's Makefile, inside the BCACHEFS_DKMS
              # block where CONFIG_BCACHEFS_FS is already set.
              sed -i 's|# Enable other features here?|# Enable other features here?\n\tCONFIG_BCACHEFS_QUOTA := y\n\tccflags-y += -DCONFIG_BCACHEFS_QUOTA|' \
                src/fs/bcachefs/Makefile
              # @NASTY_DEBUG_CHECKS_LINE@
            '';
          });
      };
    });

    mkNixosConfigs = system: let
      pkgs = mkPkgs system;
      nasty-engine = mkEngine system;
      nasty-webui = mkWebui system;
      nasty-bcachefs-tools = mkBcachefsTools system;
      installerNastyRef = "v${nasty-version}";
      installerSystemFlakeNix = builtins.replaceStrings
        [ "@NASTY_VERSION@" "@LOCAL_SYSTEM@" ]
        [ installerNastyRef system ]
        (builtins.readFile ./nixos/system-flake/flake.nix.template);
      installerSystemFlakeLock = builtins.toJSON {
        version = rootLock.version;
        root = "root";
        nodes = (builtins.removeAttrs rootLock.nodes [ "root" ]) // {
          nasty = {
            locked = {
              type = "path";
              path = self.outPath;
              narHash = self.narHash;
              lastModified = self.lastModified;
            };
            original = {
              type = "github";
              owner = installerNastyOwner;
              repo = installerNastyRepo;
              ref = installerNastyRef;
            };
            inputs = {
              bcachefs-tools = [ "bcachefs-tools" ];
              nixpkgs = [ "nixpkgs" ];
            };
          };
          root = {
            inputs = {
              bcachefs-tools = "bcachefs-tools";
              nasty = "nasty";
              nixpkgs = "nixpkgs";
            };
          };
        };
      };
      nastySystemFlakeSnapshot = pkgs.runCommand "nasty-system-flake-snapshot" {} ''
        mkdir -p "$out"
        cp ${self}/flake.nix "$out/flake.nix"
        cp ${self}/flake.lock "$out/flake.lock"
      '';
      installerSystemFlake = pkgs.runCommand "nasty-system-flake" {} ''
        mkdir -p "$out"
        cp ${./nixos/system-flake/hardware-configuration.nix} "$out/hardware-configuration.nix"
        cp ${./nixos/system-flake/networking.nix} "$out/networking.nix"
        cp ${pkgs.writeText "nasty-system-flake.nix" installerSystemFlakeNix} "$out/flake.nix"
        cp ${pkgs.writeText "nasty-system-flake.lock" installerSystemFlakeLock} "$out/flake.lock"
      '';
    in rec {
      # Full NASty appliance configuration
      nasty = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix

          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
        ];
      };

      nasty-rootfs = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
          ({ ... }: {
            boot.isContainer = true;
          })
        ];
      };

      # ISO image for installation
      nasty-iso = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = {
          inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nixpkgs;
          nasty-rootfs-toplevel = nasty-rootfs.config.system.build.toplevel;
          installerSystemFlake = installerSystemFlake;
          installerNastySource = self.outPath;
        };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./nixos/iso.nix
        ];
      };

      # Alternative ISO with systemd-boot for hardware where GRUB EFI fails
      # (e.g. ODROID H3 with JSL firmware)
      # Build: nix build .#nixosConfigurations.nasty-iso-sd.config.system.build.isoImage
      nasty-iso-sd = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = {
          inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nixpkgs;
          nasty-rootfs-toplevel = nasty-rootfs.config.system.build.toplevel;
          installerSystemFlake = installerSystemFlake;
          installerNastySource = self.outPath;
        };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./nixos/iso.nix
          ({ lib, ... }: {
            # Use systemd-boot instead of GRUB for EFI
            boot.loader.grub.enable = lib.mkForce false;
            boot.loader.systemd-boot.enable = lib.mkForce true;
          })
        ];
      };

      # QEMU VM for testing
      nasty-vm = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot; };
        modules = [
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix

          ./nixos/modules/nasty.nix
          ./nixos/configuration.nix
          ./nixos/vm.nix
        ];
      };

      # Cloud/CI disk image (Oracle Cloud compatible)
      nasty-cloud = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools nastySystemFlakeSnapshot; };
        modules = [
          "${nixpkgs}/nixos/modules/virtualisation/oci-image.nix"
          ./nixos/modules/bcachefs.nix
          ./nixos/modules/linuxquota.nix
          ./nixos/modules/nasty.nix
          ./nixos/tls.nix
          ./nixos/cloud.nix
        ];
      };
    };

  in {
    # Export packages for both architectures
    packages.x86_64-linux = {
      engine = mkEngine "x86_64-linux";
      webui = mkWebui "x86_64-linux";
      bcachefs-tools = mkBcachefsTools "x86_64-linux";
      nasty-rootfs = (mkNixosConfigs "x86_64-linux").nasty-rootfs.config.system.build.toplevel;
      nasty-cloud-image = (mkNixosConfigs "x86_64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "x86_64-linux";
    };

    packages.aarch64-linux = {
      engine = mkEngine "aarch64-linux";
      webui = mkWebui "aarch64-linux";
      bcachefs-tools = mkBcachefsTools "aarch64-linux";
      nasty-rootfs = (mkNixosConfigs "aarch64-linux").nasty-rootfs.config.system.build.toplevel;
      nasty-cloud-image = (mkNixosConfigs "aarch64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "aarch64-linux";
    };

    # NixOS module
    nixosModules = {
      nasty = ./nixos/modules/nasty.nix;
      bcachefs = ./nixos/modules/bcachefs.nix;
      linuxquota = ./nixos/modules/linuxquota.nix;
      appliance-base = ./nixos/appliance-base.nix;
    };

    # NixOS configurations for both architectures
    nixosConfigurations = (mkNixosConfigs "x86_64-linux") // (
      let configs = mkNixosConfigs "aarch64-linux"; in {
        "nasty-aarch64" = configs.nasty;
        "nasty-rootfs-aarch64" = configs.nasty-rootfs;
        "nasty-iso-aarch64" = configs.nasty-iso;
        "nasty-vm-aarch64" = configs.nasty-vm;
        "nasty-cloud-aarch64" = configs.nasty-cloud;
      }
    );
  };
}
