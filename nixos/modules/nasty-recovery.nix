{ config, lib, pkgs, ... }:

{
  config = lib.mkIf config.services.nasty.enable {
    environment.systemPackages = [ (import ../packages/recovery.nix { inherit pkgs; }) ];
  };
}
