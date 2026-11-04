#!/usr/bin/env bash
# NicoChess installer for the target machine (Debian 12). Run it as root, from the USB key:
#
#   mount /dev/sdb1 /mnt
#   bash /mnt/install.sh
#
# What it does:
#   1. installs the packages from the key (no network needed), or from the internet with --online
#   2. installs the program in /opt/nicochess and creates the user "nicochess"
#   3. creates and starts the service "nicochess" (the page is then on port 8080)
#   4. writes a report of the machine (CPU, memory, disk, camera) in /root
#
# Options:
#   --wifi-firmware   also install the firmware of common Wi-Fi dongles (Atheros, Realtek, MediaTek)
#   --online          use the Debian mirrors instead of the packages on the key
#   --demo            run the service on a synthetic board (no camera), to test the page first
#   --no-service      install the files only, do not create or start the service
#   --uninstall       remove the service and the program (games and calibration are kept)
#
# It can be run again: it then upgrades the program and keeps the settings.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/nicochess
DATA_DIR=/var/lib/nicochess
CONFIG=/etc/default/nicochess
UNIT=/etc/systemd/system/nicochess.service

WIFI=0 ONLINE=0 DEMO=0 SERVICE=1 UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --wifi-firmware) WIFI=1 ;;
    --online)        ONLINE=1 ;;
    --demo)          DEMO=1 ;;
    --no-service)    SERVICE=0 ;;
    --uninstall)     UNINSTALL=1 ;;
    -h|--help)       sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $arg (see --help)"; exit 1 ;;
  esac
done

say()  { printf '\n==== %s ====\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail "run this as root: su -   then   bash $0"

have_systemd() { command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; }

# ---- Uninstall ----------------------------------------------------------------

if [ "$UNINSTALL" -eq 1 ]; then
  say "Uninstall"
  if have_systemd; then
    systemctl disable --now nicochess 2>/dev/null
    rm -f "$UNIT"
    systemctl daemon-reload
  fi
  rm -rf "$INSTALL_DIR"
  echo "Removed the service and $INSTALL_DIR."
  echo "Kept: $DATA_DIR (games and calibration), $CONFIG, and the user nicochess."
  exit 0
fi

# ---- Check the files on the key ---------------------------------------------------

say "Checking the files"
[ -f "$HERE/nicochess" ] || fail "nicochess (the program) is not next to this script"
if [ -f "$HERE/SHA256SUMS" ]; then
  (cd "$HERE" && sha256sum -c SHA256SUMS --quiet) || fail "a file on the key is damaged: copy the folder to the key again"
  echo "All files match their checksums."
fi

# ---- Packages -----------------------------------------------------------------------

say "Packages"
export DEBIAN_FRONTEND=noninteractive
mapfile -t PACKAGES < "$HERE/packages-base.txt"
if [ "$WIFI" -eq 1 ]; then
  mapfile -t FIRMWARE < "$HERE/packages-wifi.txt"
  PACKAGES+=("${FIRMWARE[@]}")
fi
# Drop empty lines.
PACKAGES=($(printf '%s\n' "${PACKAGES[@]}" | grep -v '^$'))

if [ "$ONLINE" -eq 0 ] && [ -f "$HERE/debs/Packages" ]; then
  echo "Installing from the key (no network needed)."
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  mkdir -p "$WORK/lists/partial"
  echo "deb [trusted=yes] file:$HERE/debs ./" > "$WORK/local.list"
  # Only the repository on the key is used, and the system's own package lists are left alone.
  APT=(apt-get -o Dir::Etc::sourcelist="$WORK/local.list" -o Dir::Etc::sourceparts=/dev/null
       -o Dir::State::lists="$WORK/lists" -o APT::Get::List-Cleanup=0)
  "${APT[@]}" update || fail "cannot read the package repository on the key ($HERE/debs)"
  "${APT[@]}" install -y --no-install-recommends "${PACKAGES[@]}" || fail "package installation failed"
else
  echo "Installing from the Debian mirrors (network needed)."
  [ "$WIFI" -eq 1 ] && echo "Wi-Fi firmware needs the non-free-firmware component in /etc/apt/sources.list."
  apt-get update || fail "apt-get update failed: is the network connected?"
  apt-get install -y --no-install-recommends "${PACKAGES[@]}" || fail "package installation failed"
fi

# ---- Program, user, folders ----------------------------------------------------------

say "Program"
if ! id nicochess >/dev/null 2>&1; then
  adduser --system --group --home "$DATA_DIR" --gecos "NicoChess" nicochess >/dev/null \
    || useradd --system --user-group --home-dir "$DATA_DIR" --create-home nicochess \
    || fail "cannot create the user nicochess"
fi
usermod -aG video nicochess
mkdir -p "$INSTALL_DIR" "$DATA_DIR/games"
chown -R nicochess:nicochess "$DATA_DIR"

install -m 755 "$HERE/nicochess" "$INSTALL_DIR/nicochess.new" || fail "cannot copy the program"
"$INSTALL_DIR/nicochess.new" --help >/dev/null 2>&1 \
  || fail "the program does not run on this machine (wrong CPU type or system?)"
mv -f "$INSTALL_DIR/nicochess.new" "$INSTALL_DIR/nicochess"
echo "Program installed in $INSTALL_DIR."

# Settings that can be edited without touching the service file.
if [ ! -f "$CONFIG" ]; then
  ARGS="--width 640 --height 480 --fps 3"
  [ "$DEMO" -eq 1 ] && ARGS="--source demo"
  cat > "$CONFIG" <<EOF
# Extra options for the NicoChess service. See: /opt/nicochess/nicochess --help
# After editing, apply with: systemctl restart nicochess
NICOCHESS_ARGS="$ARGS"
EOF
  echo "Settings written to $CONFIG: $ARGS"
else
  if [ "$DEMO" -eq 1 ]; then
    sed -i 's|^NICOCHESS_ARGS=.*|NICOCHESS_ARGS="--source demo"|' "$CONFIG"
    echo "Settings in $CONFIG changed to the demo source."
  else
    echo "Settings in $CONFIG kept."
  fi
fi

# ---- Service -------------------------------------------------------------------------

if [ "$SERVICE" -eq 1 ]; then
  say "Service"
  if ! have_systemd; then
    echo "systemd is not running here: the service was not created. Run the program by hand:"
    echo "  su -s /bin/sh nicochess -c '$INSTALL_DIR/nicochess --source demo'"
  else
    cat > "$UNIT" <<EOF
[Unit]
Description=NicoChess board tracker

[Service]
User=nicochess
Group=nicochess
SupplementaryGroups=video
EnvironmentFile=-$CONFIG
ExecStart=$INSTALL_DIR/nicochess --listen 0.0.0.0:8080 --data-dir $DATA_DIR/games --calibration $DATA_DIR/calibration.json \$NICOCHESS_ARGS
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable nicochess >/dev/null 2>&1
    systemctl restart nicochess
    sleep 2
    systemctl --no-pager --lines=8 status nicochess || true
  fi
fi

# ---- Report ----------------------------------------------------------------------------

if [ -f "$HERE/target-setup.sh" ]; then
  say "Report on this machine"
  bash "$HERE/target-setup.sh" --report-only || true
fi

say "Done"
IPS="$(hostname -I 2>/dev/null)"
if [ -n "$IPS" ]; then
  for ip in $IPS; do echo "Open the page from another computer:  http://$ip:8080"; done
else
  echo "No network address yet. On this machine itself: http://localhost:8080"
fi
echo "Logs:      journalctl -u nicochess -f"
echo "Settings:  $CONFIG"
echo "Games:     $DATA_DIR/games"
echo "The report of this machine: nicochess-findings-*.txt, in /root or in the home of the first user (see \"Report saved in\" above)."
