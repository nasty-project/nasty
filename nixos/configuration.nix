{ config, lib, pkgs, nasty-engine, nasty-webui ? null, ... }:

{
  imports = [
    ./binary-cache.nix
    ./hardware-configuration.nix
    ./networking.nix
    ./tls.nix
    ./appliance-base.nix
  ];
}
