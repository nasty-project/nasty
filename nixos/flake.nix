{
  description = "NASty - NAS System built on NixOS and bcachefs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # ── bcachefs override (optional) ──────────────────────────────
    # Pinned to v1.37 release tag.
    # To revert to pure nixpkgs: comment out these two lines.
    # No other changes needed — bcachefs.nix defaults to pkgs.bcachefs-tools.
    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.2";
    bcachefs-tools.inputs.nixpkgs.follows = "nixpkgs";

  };

  outputs = { self, nixpkgs, bcachefs-tools, ... }: let
    # Helper to build packages for a given system
    mkPkgs = system: nixpkgs.legacyPackages.${system};

    mkEngine = system: let pkgs = mkPkgs system; in pkgs.rustPlatform.buildRustPackage {
      pname = "nasty-engine";
      version = "0.1.0";
      src = ../engine;
      cargoLock.lockFile = ../engine/Cargo.lock;
      meta = {
        description = "NASty NAS engine";
        license = pkgs.lib.licenses.gpl3Only;
      };
    };

    mkWebui = system: let pkgs = mkPkgs system; in pkgs.buildNpmPackage {
      pname = "nasty-webui";
      version = "0.1.0";
      src = ../webui;
      npmDepsHash = "sha256-FtC3N6WVeRPJG1LaTPckw+AP5rAC0Vf73vyTm3pxRco=";
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

    nasty-version = self.shortRev or self.dirtyShortRev or "dev";

    mkNixosConfigs = system: let
      pkgs = mkPkgs system;
      nasty-engine = mkEngine system;
      nasty-webui = mkWebui system;
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
      nasty-bcachefs-tools = let
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
    in {
      # Full NASty appliance configuration
      nasty = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools; };
        modules = [
          ./modules/bcachefs.nix
          ./modules/linuxquota.nix
          ./modules/limine
          ./modules/nasty.nix
          ./configuration.nix
        ];
      };

      # ISO image for installation
      nasty-iso = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools; };
        modules = [
          ./modules/bcachefs.nix
          ./modules/linuxquota.nix
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./iso.nix
        ];
      };

      # QEMU VM for testing
      nasty-vm = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools; };
        modules = [
          ./modules/bcachefs.nix
          ./modules/linuxquota.nix
          ./modules/limine
          ./modules/nasty.nix
          ./configuration.nix
          ./vm.nix
        ];
      };

      # Cloud/CI disk image (Oracle Cloud compatible)
      nasty-cloud = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version nasty-bcachefs-tools; };
        modules = [
          "${nixpkgs}/nixos/modules/virtualisation/oci-image.nix"
          ./modules/bcachefs.nix
          ./modules/linuxquota.nix
          ./modules/nasty.nix
          ./tls.nix
          ./cloud.nix
        ];
      };
    };

  in {
    # Export packages for both architectures
    packages.x86_64-linux = let pkgs = mkPkgs "x86_64-linux"; in {
      engine = mkEngine "x86_64-linux";
      webui = mkWebui "x86_64-linux";
      nasty-cloud-image = (mkNixosConfigs "x86_64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "x86_64-linux";
    };

    packages.aarch64-linux = let pkgs = mkPkgs "aarch64-linux"; in {
      engine = mkEngine "aarch64-linux";
      webui = mkWebui "aarch64-linux";
      nasty-cloud-image = (mkNixosConfigs "aarch64-linux").nasty-cloud.config.system.build.OCIImage;
      default = mkEngine "aarch64-linux";
    };

    # NixOS module
    nixosModules.nasty = ./modules/nasty.nix;

    # NixOS configurations for both architectures
    nixosConfigurations = (mkNixosConfigs "x86_64-linux") // (
      let configs = mkNixosConfigs "aarch64-linux"; in {
        "nasty-aarch64" = configs.nasty;
        "nasty-iso-aarch64" = configs.nasty-iso;
        "nasty-vm-aarch64" = configs.nasty-vm;
        "nasty-cloud-aarch64" = configs.nasty-cloud;
      }
    );
  };
}
