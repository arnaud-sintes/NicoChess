#!/usr/bin/env bash
# NicoChess: first setup of the target machine (Debian 12 i386).
#
# Run it ON THE TARGET, after the Debian install, with the network cable connected
# (apt needs to reach a Debian mirror):
#
#   bash target-setup.sh                   # install packages, then gather facts
#   bash target-setup.sh --wifi-firmware   # also enable non-free-firmware and install Wi-Fi dongle firmware
#   bash target-setup.sh --report-only     # gather facts only, install nothing
#
# It can be run again safely. Everything it prints is also saved in
# ~/nicochess-findings-<date>.txt of the user, so you can copy it to a USB key.
#
# It does NOT start a Wi-Fi access point and does NOT change network settings.

set -u

WIFI_FIRMWARE=0
REPORT_ONLY=0

usage() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
}

for arg in "$@"; do
  case "$arg" in
    --wifi-firmware) WIFI_FIRMWARE=1 ;;
    --report-only)   REPORT_ONLY=1 ;;
    -h|--help)       usage; exit 0 ;;
    *) echo "Unknown option: $arg"; usage; exit 1 ;;
  esac
done

# ---- Root and user ----------------------------------------------------------

if [ "$(id -u)" -ne 0 ]; then
  if command -v sudo >/dev/null 2>&1; then
    exec sudo -E bash "$0" "$@"
  fi
  echo "This script needs root. Run it with: su -c 'bash $0'   (or install sudo first)"
  exit 1
fi

REAL_USER="${SUDO_USER:-}"
if [ -z "$REAL_USER" ] || [ "$REAL_USER" = "root" ]; then
  # First normal user on the machine.
  REAL_USER="$(getent passwd | awk -F: '$3>=1000 && $3<65534 {print $1; exit}')"
fi
if [ -n "$REAL_USER" ]; then
  REAL_HOME="$(getent passwd "$REAL_USER" | cut -d: -f6)"
else
  REAL_HOME="/root"
fi

REPORT="$REAL_HOME/nicochess-findings-$(date +%Y%m%d-%H%M%S).txt"
exec > >(tee -a "$REPORT") 2>&1

section() { printf '\n==== %s ====\n' "$1"; }
run()     { printf '$ %s\n' "$*"; "$@" 2>&1; }

echo "NicoChess target setup, $(date)"
echo "User: ${REAL_USER:-<none found>}   Report: $REPORT"

# ---- Install ----------------------------------------------------------------

add_nonfree_firmware() {
  # Adds the non-free-firmware component to the Debian "deb" lines of apt sources.
  local f
  for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list; do
    [ -f "$f" ] || continue
    if grep -qE '^deb .* main' "$f" && ! grep -q 'non-free-firmware' "$f"; then
      cp -a "$f" "$f.bak-nicochess"
      sed -i -E '/^deb /{/non-free-firmware/!s/ main( |$)/ main non-free-firmware\1/}' "$f"
      echo "Added non-free-firmware to $f (backup: $f.bak-nicochess)"
    fi
  done
}

if [ "$REPORT_ONLY" -eq 0 ]; then
  section "Install packages"

  if [ "$WIFI_FIRMWARE" -eq 1 ]; then
    add_nonfree_firmware
  fi

  export DEBIAN_FRONTEND=noninteractive
  run apt-get update || { echo "apt-get update failed: is the network cable connected?"; exit 1; }

  PACKAGES=(
    openssh-server   # remote access from the PC
    v4l-utils        # v4l2-ctl: camera formats and controls
    usbutils         # lsusb
    pciutils         # lspci
    rsync            # deploy binaries
    curl ca-certificates
    iw               # check Wi-Fi access point support
    hostapd dnsmasq  # Wi-Fi access point (installed, not enabled)
  )
  if [ "$WIFI_FIRMWARE" -eq 1 ]; then
    PACKAGES+=(firmware-atheros firmware-realtek firmware-misc-nonfree)
  fi

  run apt-get install -y --no-install-recommends "${PACKAGES[@]}" || echo "WARNING: some packages failed to install, see above."

  section "Services"
  run systemctl enable --now ssh
  # Installed for later use, but not started: no access point yet.
  systemctl disable --now dnsmasq >/dev/null 2>&1 && echo "dnsmasq disabled (not configured yet)"
  systemctl disable --now hostapd >/dev/null 2>&1 && echo "hostapd disabled (not configured yet)"

  section "User and system settings"
  if [ -n "$REAL_USER" ]; then
    run usermod -aG video "$REAL_USER"
    echo "User $REAL_USER added to the video group (log out and back in to apply)."
  else
    echo "No normal user found: video group not set."
  fi

  mkdir -p /etc/systemd/journald.conf.d
  printf '[Journal]\nSystemMaxUse=50M\n' > /etc/systemd/journald.conf.d/nicochess-size.conf
  systemctl restart systemd-journald 2>/dev/null
  echo "Journal size limited to 50 MB."
fi

# ---- Gather facts -----------------------------------------------------------

section "System"
run cat /etc/os-release
run uname -a
echo "dpkg architecture: $(dpkg --print-architecture)   getconf LONG_BIT: $(getconf LONG_BIT)"
run date
run timedatectl

section "CPU"
run lscpu
for flag in pae sse2 sse3 lm; do
  if grep -qw "$flag" /proc/cpuinfo; then echo "flag $flag: present"; else echo "flag $flag: absent"; fi
done
echo "(expected on an Atom N270: pae present, sse2 present, lm absent = 32-bit only)"

section "Memory and storage"
run free -m
run lsblk
run df -h /
echo "Storage errors in the kernel log (should be empty):"
dmesg 2>/dev/null | grep -i -E "I/O error|medium error|EXT4-fs error|ata[0-9.]+: .*(error|exception)" | tail -20

section "Network"
run ip -br a

section "USB devices"
run lsusb

section "Camera"
if command -v v4l2-ctl >/dev/null 2>&1; then
  run v4l2-ctl --list-devices
  found=0
  for dev in /dev/video*; do
    [ -e "$dev" ] || continue
    if v4l2-ctl -d "$dev" --list-formats-ext 2>/dev/null | grep -q 'Pixel Format'; then
      found=1
      printf '\n-- %s --\n' "$dev"
      run v4l2-ctl -d "$dev" --list-formats-ext
      run v4l2-ctl -d "$dev" --list-ctrls
    fi
  done
  [ "$found" -eq 1 ] || echo "No capture device found: is the webcam plugged in? (check lsusb above)"
else
  echo "v4l2-ctl not installed (run without --report-only)."
fi

section "Wi-Fi"
if command -v iw >/dev/null 2>&1; then
  if iw dev 2>/dev/null | grep -q Interface; then
    run iw dev
    echo "Supported interface modes (\"AP\" is needed for the access point):"
    iw list 2>/dev/null | grep -A 12 "Supported interface modes"
  else
    echo "No Wi-Fi interface found (dongle not plugged in, or firmware missing: try --wifi-firmware)."
    dmesg 2>/dev/null | grep -i -E "firmware|usb.*(wlan|wifi|802)" | tail -10
  fi
else
  echo "iw not installed (run without --report-only)."
fi

# ---- End --------------------------------------------------------------------

[ -n "$REAL_USER" ] && chown "$REAL_USER":"$REAL_USER" "$REPORT" 2>/dev/null

section "Done"
echo "Report saved in: $REPORT"
IP="$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
[ -n "$IP" ] && echo "From the PC:  ssh ${REAL_USER:-user}@$IP"
echo "Copy the report to a USB key (see PLAN.md) or send its content back to continue."
