#!/bin/sh
# Runs INSIDE a Debian 12 i386 container (see scripts/make-usb-bundle.ps1).
# Downloads the packages in packages-base.txt and packages-wifi.txt, with everything they
# depend on, into /out, and turns /out into a small local apt repository that the target
# can install from with no network.
#
# The whole dependency closure is downloaded, not only what this container lacks: the
# container and the target may have different versions of the base libraries, and a set
# taken from one consistent snapshot installs cleanly on the other.

set -eu

# Wi-Fi firmware lives in a component that is not enabled by default.
for f in /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list; do
  [ -f "$f" ] && sed -i 's/^Components: main$/Components: main non-free-firmware/; s/^\(deb .* main\)$/\1 non-free-firmware/' "$f"
done

apt-get update -qq
apt-get install -y -qq --no-install-recommends apt-utils >/dev/null

PACKAGES="$(cat /scripts/packages-base.txt /scripts/packages-wifi.txt | tr '\n' ' ')"

# Package names only (lines that start in column 0), without the <virtual> ones.
apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts \
  --no-breaks --no-replaces --no-enhances $PACKAGES \
  | grep '^[a-z0-9]' | sort -u > /tmp/closure.txt
echo "closure: $(wc -l < /tmp/closure.txt) names"

cd /out
rm -f ./*.deb Packages Release
for p in $(cat /tmp/closure.txt); do
  apt-get download -qq "$p" 2>/dev/null || echo "skipped (no candidate, probably virtual): $p"
done

apt-ftparchive packages . > Packages
apt-ftparchive release . > Release
echo "downloaded: $(ls ./*.deb | wc -l) packages, $(du -sh . | cut -f1)"
