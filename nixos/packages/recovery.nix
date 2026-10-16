{ pkgs }:

let
  script = pkgs.writeText "recovery-inspect.py" (builtins.readFile ../scripts/recovery-inspect.py);
in pkgs.writeShellApplication {
  name = "nasty-recovery";
  text = ''
    exec ${pkgs.python3}/bin/python3 ${script} "$@"
  '';
}
