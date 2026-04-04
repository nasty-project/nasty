{ ... }:

{
  # Binary cache
  nix.settings = {
    substituters = [ "https://nasty.cachix.org" ];
    trusted-public-keys = [ "nasty.cachix.org-1:s+X88yw6+asphCNphTId/RQZHfmDF4fQ0uyzEz5SxLc=" ];
  };
}
