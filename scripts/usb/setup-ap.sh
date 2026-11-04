#!/usr/bin/env bash
# Turns a USB Wi-Fi dongle into an access point, so a phone or tablet can join the
# machine directly and open the NicoChess page. Run it as root, after install.sh:
#
#   bash setup-ap.sh --ssid NicoChess --pass 'a long passphrase' --country FR
#
# Options:
#   --ssid NAME       network name                          [default: NicoChess]
#   --pass TEXT       WPA2 passphrase, 8 to 63 characters   [default: a random one, printed at the end]
#   --country CODE    two-letter country code (radio rules) [default: FR]
#   --channel N       2.4 GHz channel, 1 to 11              [default: 6]
#   --iface NAME      Wi-Fi interface                       [default: the first one that can be an access point]
#   --address IP      address of this machine on the Wi-Fi  [default: 192.168.4.1]
#   --dry-run         write the files to a temporary folder and show them, change nothing
#   --yes             do not ask for confirmation
#   --remove          stop and remove what this script created
#
# After it, a phone joins the network and opens  http://192.168.4.1:8080
#
# NOT TESTED with a real dongle yet. It uses its own systemd service (nicochess-ap) and
# does not rely on the Debian hostapd service.
#
# WARNING: an access point inside a company building can be flagged as a rogue access point.
# Use it where it is allowed.

set -u

SSID=NicoChess PASS="" COUNTRY=FR CHANNEL=6 IFACE="" ADDRESS=192.168.4.1 DRY=0 YES=0 REMOVE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --ssid) SSID="${2:-}"; shift ;;
    --pass) PASS="${2:-}"; shift ;;
    --country) COUNTRY="${2:-}"; shift ;;
    --channel) CHANNEL="${2:-}"; shift ;;
    --iface) IFACE="${2:-}"; shift ;;
    --address) ADDRESS="${2:-}"; shift ;;
    --dry-run) DRY=1 ;;
    --yes) YES=1 ;;
    --remove) REMOVE=1 ;;
    -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "Unknown option: $1 (see --help)"; exit 1 ;;
  esac
  shift
done

fail() { echo "ERROR: $*" >&2; exit 1; }
[ "$DRY" -eq 1 ] || [ "$(id -u)" -eq 0 ] || fail "run this as root: su -   then   bash $0"

if [ "$REMOVE" -eq 1 ] && [ "$DRY" -eq 0 ]; then
  systemctl disable --now nicochess-ap dnsmasq 2>/dev/null
  rm -f /etc/systemd/system/nicochess-ap.service /etc/hostapd/nicochess-ap.conf \
        /etc/dnsmasq.d/nicochess.conf /etc/systemd/system/dnsmasq.service.d/nicochess.conf \
        /etc/NetworkManager/conf.d/nicochess-unmanaged.conf
  systemctl daemon-reload
  echo "Access point removed."
  exit 0
fi

# ---- Checks -----------------------------------------------------------------------

case "$CHANNEL" in ''|*[!0-9]*) fail "--channel must be a number" ;; esac
[ "$CHANNEL" -ge 1 ] && [ "$CHANNEL" -le 11 ] || fail "--channel must be between 1 and 11"
case "$COUNTRY" in [A-Za-z][A-Za-z]) COUNTRY="$(echo "$COUNTRY" | tr a-z A-Z)" ;; *) fail "--country must be two letters" ;; esac
[ -n "$SSID" ] && [ "${#SSID}" -le 32 ] || fail "--ssid must be 1 to 32 characters"
GENERATED=0
if [ -z "$PASS" ]; then
  PASS="$(tr -dc 'a-km-np-z2-9' < /dev/urandom | head -c 12)"; GENERATED=1
fi
[ "${#PASS}" -ge 8 ] && [ "${#PASS}" -le 63 ] || fail "the passphrase must be 8 to 63 characters"
case "$PASS$SSID" in *\"*|*\\*) fail "the name and the passphrase must not contain quotes or backslashes" ;; esac

if [ "$DRY" -eq 0 ]; then
  command -v hostapd >/dev/null || fail "hostapd is not installed (run install.sh first)"
  command -v dnsmasq >/dev/null || fail "dnsmasq is not installed (run install.sh first)"
  command -v iw >/dev/null || fail "iw is not installed (run install.sh first)"

  if [ -z "$IFACE" ]; then
    for candidate in $(iw dev 2>/dev/null | awk '$1=="Interface"{print $2}'); do
      phy="phy$(iw dev "$candidate" info | awk '/wiphy/{print $2}')"
      if iw "$phy" info 2>/dev/null | sed -n '/Supported interface modes/,/Band/p' | grep -q '\* AP$'; then
        IFACE="$candidate"; break
      fi
    done
  fi
  [ -n "$IFACE" ] || fail "no Wi-Fi interface that can be an access point. Is the dongle plugged in? Does it need firmware (install.sh --wifi-firmware)? See: iw dev, dmesg | tail"
  echo "Using the Wi-Fi interface $IFACE."

  if [ "$YES" -eq 0 ]; then
    echo "This starts a Wi-Fi access point named \"$SSID\". Inside a company building it can be flagged as"
    echo "a rogue access point: use it only where it is allowed."
    read -r -p "Continue? [y/N] " answer
    case "$answer" in y|Y|yes|YES) ;; *) echo "Cancelled."; exit 1 ;; esac
  fi
else
  IFACE="${IFACE:-wlan0}"
  ROOT="$(mktemp -d)"
fi
ROOT="${ROOT:-}"

NETWORK="${ADDRESS%.*}"
write() { # write FILE : content on stdin
  mkdir -p "$ROOT$(dirname "$1")"; cat > "$ROOT$1"
}

# ---- The files ------------------------------------------------------------------------

write /etc/hostapd/nicochess-ap.conf <<EOF
interface=$IFACE
driver=nl80211
ssid=$SSID
country_code=$COUNTRY
hw_mode=g
channel=$CHANNEL
ieee80211n=1
wmm_enabled=1
auth_algs=1
wpa=2
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
wpa_passphrase=$PASS
EOF
[ "$DRY" -eq 0 ] && chmod 600 /etc/hostapd/nicochess-ap.conf

write /etc/systemd/system/nicochess-ap.service <<EOF
[Unit]
Description=NicoChess Wi-Fi access point
After=network.target

[Service]
ExecStartPre=-/bin/sh -c 'rfkill unblock wlan; iw reg set $COUNTRY'
ExecStartPre=/bin/sh -c 'ip link set $IFACE up && ip addr replace $ADDRESS/24 dev $IFACE'
ExecStart=/usr/sbin/hostapd /etc/hostapd/nicochess-ap.conf
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

write /etc/dnsmasq.d/nicochess.conf <<EOF
# DHCP for the phones on the access point, and the name nicochess.lan.
interface=$IFACE
bind-interfaces
dhcp-range=$NETWORK.10,$NETWORK.50,255.255.255.0,12h
address=/nicochess.lan/$ADDRESS
EOF

# dnsmasq needs the interface to have its address first.
write /etc/systemd/system/dnsmasq.service.d/nicochess.conf <<EOF
[Unit]
After=nicochess-ap.service
Wants=nicochess-ap.service
EOF

if [ "$DRY" -eq 1 ] || [ -d /etc/NetworkManager/conf.d ]; then
  # A desktop's network manager would otherwise take the interface back.
  write /etc/NetworkManager/conf.d/nicochess-unmanaged.conf <<EOF
[keyfile]
unmanaged-devices=interface-name:$IFACE
EOF
fi

# ---- Show or apply ----------------------------------------------------------------------

if [ "$DRY" -eq 1 ]; then
  echo "Dry run: files written under $ROOT"
  (cd "$ROOT" && find . -type f | sort | while read -r f; do echo; echo "--- ${f#.} ---"; cat "$f"; done)
  rm -rf "$ROOT"
  exit 0
fi

# Debian's own hostapd service is not used: ours has the address and the order right.
systemctl disable --now hostapd 2>/dev/null
systemctl daemon-reload
command -v nmcli >/dev/null && nmcli general reload 2>/dev/null
systemctl enable --now nicochess-ap || fail "the access point did not start: journalctl -u nicochess-ap"
systemctl enable dnsmasq >/dev/null 2>&1
systemctl restart dnsmasq || fail "dnsmasq did not start: journalctl -u dnsmasq"
sleep 2
systemctl --no-pager --lines=5 status nicochess-ap || true

echo
echo "Access point ready."
echo "  Network:     $SSID"
if [ "$GENERATED" -eq 1 ]; then echo "  Passphrase:  $PASS   (generated: write it down)"; else echo "  Passphrase:  as given"; fi
echo "  Page:        http://$ADDRESS:8080   or   http://nicochess.lan:8080"
echo "Remove it with: bash $0 --remove"
