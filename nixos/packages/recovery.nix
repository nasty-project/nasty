{ pkgs }:

let
  cacert = pkgs.cacert.override { blacklist = import ./distrusted-cas.nix; };
  script = pkgs.writeText "recovery-inspect.py" (builtins.replaceStrings
    [ "@rustic@" "@ca_bundle@" ]
    [ "${pkgs.rustic}/bin/rustic" "${cacert}/etc/ssl/certs/ca-bundle.crt" ]
    (builtins.readFile ../scripts/recovery-inspect.py));
in pkgs.writeShellApplication {
  name = "nasty-recovery";
  text = ''
    exec ${pkgs.python3}/bin/python3 ${script} "$@"
  '';
}
