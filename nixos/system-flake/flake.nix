{
  description = "NASty local system configuration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.4";
    bcachefs-tools.inputs.nixpkgs.follows = "nixpkgs";

    # The installed appliance tracks the upstream NASty flake and keeps only
    # machine-local overlay files under /etc/nixos/nixos.
    nasty.url = "github:nasty-project/nasty/main";
    nasty.inputs.nixpkgs.follows = "nixpkgs";
    nasty.inputs.bcachefs-tools.follows = "bcachefs-tools";
  };

  outputs = { nixpkgs, nasty, ... }:
    let
      nasty-version =
        (builtins.fromTOML (builtins.readFile "${nasty}/engine/Cargo.toml")).workspace.package.version;

      mkSystem = system: nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = {
          nasty-engine = nasty.packages.${system}.engine;
          nasty-webui = nasty.packages.${system}.webui;
          nasty-version = nasty-version;
          nasty-bcachefs-tools = nasty.packages.${system}.bcachefs-tools;
        };
        modules = [
          "${nasty}/nixos/binary-cache.nix"
          "${nasty}/nixos/modules/bcachefs.nix"
          "${nasty}/nixos/modules/linuxquota.nix"
          "${nasty}/nixos/modules/nasty.nix"
          "${nasty}/nixos/appliance-base.nix"
          # Machine-local overlay files live directly under /etc/nixos.
          ./hardware-configuration.nix
          ./networking.nix
        ];
      };
    in {
      nixosConfigurations = {
        nasty = mkSystem "x86_64-linux";
        nasty-aarch64 = mkSystem "aarch64-linux";
      };
    };
}
