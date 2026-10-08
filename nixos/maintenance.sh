# Packaged by writeShellApplication as nasty-maintenance.
set -euo pipefail

flag=/var/lib/nasty/maintenance
active=/run/nasty-maintenance

usage() {
  echo "Usage: nasty-maintenance {status|enter|exit} [--no-reboot]" >&2
}

if [[ $# -lt 1 || $# -gt 2 ]]; then usage; exit 2; fi
action=$1
if [[ $# -eq 2 && $2 != --no-reboot ]]; then usage; exit 2; fi

case "$action" in
  status)
    if [[ -e $active ]]; then
      echo "Storage maintenance is active. The normal dashboard and storage consumers are disabled."
      if [[ ! -e $flag ]]; then echo "Normal startup is scheduled for the next reboot."; fi
    elif [[ -e $flag ]]; then
      echo "Storage maintenance is scheduled for the next reboot; pools may still be mounted."
    else
      echo "Normal operation."
    fi
    exit 0
    ;;
  enter|exit) ;;
  *) usage; exit 2 ;;
esac

if [[ $EUID -ne 0 ]]; then echo "Run this command as root." >&2; exit 1; fi
if [[ ! -d /var/lib/nasty ]]; then install -d -m 0751 /var/lib/nasty; fi
# The flag must survive without any data pool being mounted.
state_path=$(readlink -f /var/lib/nasty)
state_mount=$(findmnt -n -o TARGET --target /var/lib/nasty)
state_type=$(findmnt -n -o FSTYPE --target /var/lib/nasty)
state_source=$(findmnt -n -o SOURCE --target /var/lib/nasty)
root_source=$(findmnt -n -o SOURCE --target /)
if [[ $state_path == /fs || $state_path == /fs/* || $state_mount == /fs || $state_mount == /fs/* || ( $state_type == bcachefs && $state_source != "$root_source" ) ]]; then
  echo "Maintenance state must be on the OS filesystem, not a data pool." >&2
  exit 1
fi
exec 9>/run/nasty-maintenance.lock
flock -x 9

if [[ $action == enter ]]; then
  if ! systemctl cat sshd.service >/dev/null 2>&1; then
    echo "SSH must be configured before entering maintenance; the normal dashboard will be unavailable." >&2
    exit 1
  fi
  temporary=$(mktemp /var/lib/nasty/.maintenance.XXXXXX)
  trap 'rm -f "$temporary"' EXIT
  printf 'Storage maintenance requested\n' > "$temporary"
  chmod 0600 "$temporary"
  sync -f "$temporary"
  mv -T "$temporary" "$flag"
  sync -f /var/lib/nasty
  echo "Maintenance scheduled. Pools are not offline until the reboot completes."
else
  rm -f "$flag"
  sync -f /var/lib/nasty
  echo "Normal startup scheduled. Maintenance guards remain active until reboot."
fi

if [[ ${2:-} != --no-reboot ]]; then
  # Delay shutdown long enough for the WebUI RPC response to arrive. The durable
  # flag is already saved; a scheduling failure leaves it available for retry.
  systemd-run --unit="nasty-maintenance-reboot-$$" --collect \
    --on-active=3s --timer-property=AccuracySec=1s systemctl reboot
  echo "Reboot scheduled."
fi
