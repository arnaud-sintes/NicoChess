NICOCHESS - USB KEY
===================

What is on this key
-------------------
  nicochess          the program (32-bit, for Debian 12 on the INEOS 600)
  install.sh         installs everything on the target, from this key (no network needed)
  setup-ap.sh        turns a USB Wi-Fi dongle into an access point (optional, not tested yet)
  target-setup.sh    reports on the machine: CPU, memory, disk, camera, Wi-Fi
  debs/              the Debian packages needed, as a small offline repository
  docs/              the plan and the requirements, for reference
  SHA256SUMS         checksums, checked by install.sh

On the target
-------------
1. Log in, become root:           su -
2. Plug in the key, find it:      lsblk                 (the key is probably sdb1; sda is the disk)
3. Mount it:                      mount /dev/sdb1 /mnt
4. Install, with the demo first:  bash /mnt/install.sh --demo

   The demo plays a whole game on a synthetic board, so the page can be tried without the camera.
   The installer prints the address of the page at the end: http://<address>:8080
   Open it from another computer on the same network. The first one to connect is the "controller".

5. Try the demo (see "First test" below), then switch to the real camera:
      /opt/nicochess/nicochess --probe              what the camera offers (formats, controls)
      nano /etc/default/nicochess                   set:  NICOCHESS_ARGS="--width 640 --height 480 --fps 3"
      systemctl restart nicochess
      journalctl -u nicochess -f                    the program's messages (lines starting "camera:")

6. Take the key back with the results:
      cp /root/nicochess-findings-*.txt /mnt/       the report of the machine
      cp /var/lib/nicochess/games/*.json /mnt/      the recorded games (if any)
      umount /mnt                                   always, before pulling the key out

First test (demo)
-----------------
  - Open the page, go to "Calibrate", press "Open the camera".
  - Click the 4 corners of the board in this order: a1, h1, h8, a8. On the demo image they are
    near (120,440), (520,425), (470,90), (190,100) in the 640x480 picture: bottom-left, bottom-right,
    top-right, top-left of the board.
  - Go to "Live", press "Start game". The demo plays 33 moves, then the game ends by checkmate.
  - "Games" lists it, and the replay plays it back. "PGN" downloads it.

Wi-Fi access point (optional, when the dongle is plugged in)
-----------------------------------------------------------
      bash /mnt/install.sh --wifi-firmware           the firmware of common dongles
      bash /mnt/setup-ap.sh --ssid NicoChess --country FR
  A phone joins the network and opens  http://192.168.4.1:8080
  Use it only where an access point is allowed (not on a company network).
  To try the configuration without changing anything: bash /mnt/setup-ap.sh --dry-run

Other things
------------
  Update the program:   put the new nicochess and SHA256SUMS on the key, run install.sh again.
  Settings:             /etc/default/nicochess  (options: /opt/nicochess/nicochess --help)
  Stop / start:         systemctl stop nicochess / systemctl start nicochess
  Remove:               bash /mnt/install.sh --uninstall   (games and calibration are kept)
  Network available?    bash /mnt/install.sh --online      (uses the Debian mirrors instead of the key)

If something goes wrong
-----------------------
  "a file on the key is damaged"    copy the folder to the key again
  "the program does not run"        the machine is not 32-bit x86 Debian 12: send the report
  the page does not open            systemctl status nicochess ; ip -br a ; the firewall is not used
  "No capture device found"         plug the camera in; ls /dev/video* ; /opt/nicochess/nicochess --probe
  no Wi-Fi interface for the AP     iw dev ; dmesg | tail ; try install.sh --wifi-firmware, replug the dongle
