{
  description = "NASty local system configuration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    bcachefs-tools.url = "github:koverstreet/bcachefs-tools/v1.37.4";
    bcachefs-tools.inputs.nixpkgs.follows = "nixpkgs";

    # Tracks this migration branch for now so test installs boot the same code
    # that built the ISO. Once merged, switch this ref back to the upstream main
    # branch and refresh flake.lock.
    nasty.url = "github:peigongdsd/nasty/system-configuration-slim";
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
          "${nasty}/nixos/modules/bcachefs.nix"
          "${nasty}/nixos/modules/linuxquota.nix"
          "${nasty}/nixos/modules/nasty.nix"
          "${nasty}/nixos/appliance-base.nix"
          ./nixos/hardware-configuration.nix
          ./nixos/networking.nix
          ./nixos/tls.nix
        ];
      };
    in {
      nixosConfigurations = {
        nasty = mkSystem "x86_64-linux";
        nasty-aarch64 = mkSystem "aarch64-linux";
      };
    };
}
