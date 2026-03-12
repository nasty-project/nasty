{
  description = "NASty - NAS System built on NixOS and bcachefs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }: let
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
      npmDepsHash = "sha256-gNdGIPgAvUU7jsXnRQr+0FVG+HoHTO8FbCy4++SCSPY=";
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
      nasty-engine = mkEngine system;
      nasty-webui = mkWebui system;
    in {
      # Full NASty appliance configuration
      nasty = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version; };
        modules = [
          ./modules/nasty.nix
          ./configuration.nix
        ];
      };

      # ISO image for installation
      nasty-iso = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version; };
        modules = [
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./iso.nix
        ];
      };

      # QEMU VM for testing
      nasty-vm = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit nasty-engine nasty-webui nasty-version; };
        modules = [
          ./modules/nasty.nix
          ./configuration.nix
          ./vm.nix
        ];
      };
    };

  in {
    # Export packages for both architectures
    packages.x86_64-linux = let pkgs = mkPkgs "x86_64-linux"; in {
      engine = mkEngine "x86_64-linux";
      webui = mkWebui "x86_64-linux";
      default = mkEngine "x86_64-linux";
    };

    packages.aarch64-linux = let pkgs = mkPkgs "aarch64-linux"; in {
      engine = mkEngine "aarch64-linux";
      webui = mkWebui "aarch64-linux";
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
      }
    );
  };
}
