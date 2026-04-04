{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./hardware-configuration.nix
    ./networking.nix
    ./tls.nix
    ./appliance-base.nix
  ];
}
