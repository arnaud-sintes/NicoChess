# Plan: target hardware setup

Goal: get the INEOS 600 (Atom N270) to a state where it runs a headless NicoChess binary, reads the webcam, and serves a preview over HTTP.

Background and constraints are in [docs/target-hardware.md](docs/target-hardware.md).
Items marked **(verify)** come from memory and must be checked before relying on them.

Conventions:
- `PC` = the main Windows development machine.
- `target` = the INEOS 600.
- Commands without a prefix run on the target, over SSH once step 3 is done.

## What you need

- A USB stick of 2 GB or more.
- A wired network connection for the target (Ethernet, DHCP) for the install and setup. If the corporate network does not allow a new device, do the install at home.
- Later, a USB Wi-Fi dongle that supports access point mode (see section 8). It is not needed for the install.
- A keyboard and a monitor for the install only. After that the target is headless.
- The Logitech C170 and a tripod.
- Rufus or balenaEtcher on the PC to write the USB stick.

## 0. Already downloaded on the PC

Everything below is in `downloads/` (git-ignored). Checksums were verified on 2026-11-03.

| What | Where | Check done |
|---|---|---|
| Debian 12.15.0 i386 netinst ISO (656 MB) | `downloads/debian-12.15.0-i386/debian-12.15.0-i386-netinst.iso` | SHA256 matches `SHA256SUMS`, and the `SHA256SUMS.sign` signature is good for the Debian CD signing key (fingerprint `DF9B 9C49 EAA9 2984 3258 9D76 DA87 E80D 6294 BE9B`) |
| Rufus 4.15 portable | `downloads/rufus-4.15/rufus-4.15.exe` | Not verified (a `.sig` file is next to it) |

The program is built in a Docker container (section 5), so the PC needs no cross-compile toolchain. Zig and `cargo-zigbuild` were downloaded for an earlier route that did not work with the camera crate; they are not used (the folder `downloads/tools/` can be deleted).

Not downloaded:
- `ffmpeg`, used in step 4.3 to view a raw test capture on the PC. Install it with `winget install ffmpeg` or use WSL (`sudo apt install ffmpeg`).
- Nothing for the camera: the C170 is UVC and needs no driver on Linux.

## 1. Write the USB stick and inspect the target

The ISO is a **netinst**: it installs over the network, so the target needs a wired connection during the install.
There is no live system, and the installer has no `v4l-utils`, so the inspection is split: CPU, RAM and disk now, camera after the install (step 4).

1. Insert the USB stick and run `downloads/rufus-4.15/rufus-4.15.exe`.
   Pick the ISO, check that the device shown is the USB stick (everything on it is erased), and choose **DD image mode** when asked.
2. Boot the target from the USB stick (the BIOS boot menu key varies, often F2, F10, F11, F12 or Del) **(verify)**.
3. At the installer menu choose "Install" (not the graphical one), and before going on, open the second console (Ctrl+Alt+F2, then Enter) and run:
   ```
   cat /proc/cpuinfo                  # "model name", and the flags line: look for pae, sse2, sse3, no "lm"
   free -m                            # real RAM (busybox, so no -h)
   cat /proc/partitions               # real storage size
   ip -br a                           # network interface is up and has an address
   ```
   Some of these tools may be missing in the installer's busybox shell; if so, note it and check after the install.
   Go back to the installer with Ctrl+Alt+F1.

Write the results down (a note of your own, or keep the report that `scripts/target-setup.sh` produces, `nicochess-findings-<date>.txt`, next to the project).

Decision points from these results:

| Finding | Consequence |
|---|---|
| `pae` flag missing | The default i386 kernel needs PAE. Install `linux-image-686` instead of `linux-image-686-pae` **(verify package names)**. |
| `sse2` flag missing | The `i686-*` Rust targets assume SSE2, so the plan needs another target. Unlikely on an N270, which has SSE3. |
| Camera offers only MJPEG at 640×480 | Acceptable. Decoding 640×480 is cheap. |
| Camera offers YUYV 640×480 | Best case, use it. |
| `v4l2-ctl --list-ctrls` shows no manual exposure or white balance | Auto-exposure drift cannot be disabled. Plan more careful lighting and tolerance in the stability gate. |
| RAM is 512 MB or less | Add a swap file and avoid any package beyond the minimum list below. |

## 2. Install Debian 12 (i386), headless

Debian 12 is recommended in `docs/target-hardware.md`, because Debian 13 dropped i386.
Regular security support for Debian 12 ended in mid-2026 and it should now be in LTS until June 2028 **(verify on debian.org, including that LTS covers i386)**.

Installer choices (text installer, "Install", not graphical):

| Prompt | Choice |
|---|---|
| Language, location, keyboard | As preferred. |
| Hostname | `nicochess` |
| Domain | Leave empty. |
| Root password | Leave empty to disable root login and get `sudo` for the first user. |
| User | Create a normal user, for example `nico`. |
| Partitioning | Guided, use entire disk, all files in one partition. |
| Package mirror | Choose a nearby one. |
| Popularity contest | No. |
| Software selection | **Untick** "Debian desktop environment" and "GNOME". **Tick** "SSH server" and "standard system utilities". |
| Bootloader (GRUB) | Install to the disk's boot device. |

### If "Install the base system" fails

Read the installer log. The installer has virtual consoles:

| Keys | Shows |
|---|---|
| Alt+F1 (or Ctrl+Alt+F1) | The installer menus |
| Alt+F2, then Enter | A shell (busybox) |
| Alt+F3 | Main installer log |
| Alt+F4 | Live system log (syslog) |

Try Alt+Fn first; if nothing happens, use Ctrl+Alt+Fn. On some keyboards you also need to hold Fn.

If you are stuck in the menus, another way is through the menu itself:
1. On the error screen choose **Go Back**, and keep going back until the main installer menu list appears.
2. Choose **Execute a shell** (or **Save debug logs** to copy the logs to a USB stick or the network).

In the shell, run:

```
date                                                   # a wrong year breaks package signature checks
grep -i -E "error|fail|W:|E:" /var/log/syslog | tail -30
free -m
cat /proc/partitions
```

To go back to the installer, type `exit`, or press Alt+F1.

Likely causes:

| Cause | Sign | Fix |
|---|---|---|
| Wrong date (dead BIOS battery) | `Release file ... is not valid yet`, old year in `date` | Set it in the BIOS, or `date -s "YYYY-MM-DD HH:MM"` in the shell |
| Network or mirror blocked (corporate proxy) | `Failed to retrieve`, `Release`, `404` or timeouts | Install at home, or pick another mirror |
| Bad USB write, stick or port | I/O errors, corrupt package | Re-write in DD mode, use another stick or port |
| Disk too small or faulty | `No space left`, disk I/O errors | Check `/proc/partitions`, partition again |
| Too little RAM | `Out of memory`, `killed` | Check `free -m` |

When the installer finishes, remove the USB stick and reboot.
Check the target boots into a login prompt and note its IP address (`ip -br a`).

## 3. First configuration

### 3.1 SSH access from the PC

From the PC (Windows 10/11 has OpenSSH):

```
ssh nico@<target-ip>
```

Then set up key login so no password is needed.
On the PC, if you have no key yet:

```
ssh-keygen -t ed25519
type $env:USERPROFILE\.ssh\id_ed25519.pub | ssh nico@<target-ip> "mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys && chmod 700 ~/.ssh && chmod 600 ~/.ssh/authorized_keys"
```

(The second command is PowerShell.) Test that `ssh nico@<target-ip>` no longer asks for a password.

Once the key works, disable password login on the target:

```
sudo sed -i 's/^#\?PasswordAuthentication .*/PasswordAuthentication no/' /etc/ssh/sshd_config
sudo systemctl reload ssh
```

Keep your current SSH session open and test a second one before closing it, in case of a mistake.

### 3.2 Stable address

Preferred: make a DHCP reservation for the target's MAC address on the router, so no configuration is needed on the target.
Alternative: set a static address in `/etc/network/interfaces` (the Debian minimal install uses ifupdown) **(verify on the installed system)**.

Optionally add a line to the PC's hosts file so `nicochess` resolves to the reserved address.

### 3.3 Packages

The script `scripts/target-setup.sh` does this step and gathers the facts the plan needs (CPU flags, RAM, disk errors, camera formats and controls, Wi-Fi AP support) into `~/nicochess-findings-<date>.txt`.
Copy it to the target on a USB key, with the network cable connected, then run:

```
bash target-setup.sh                   # packages + report
bash target-setup.sh --wifi-firmware   # also non-free Wi-Fi dongle firmware
bash target-setup.sh --report-only     # report only, install nothing
```

It installs `openssh-server`, `v4l-utils`, `usbutils`, `pciutils`, `rsync`, `curl`, `iw`, `hostapd` and `dnsmasq`, adds the user to the `video` group, and limits the journal to 50 MB. `hostapd` and `dnsmasq` are installed but left disabled.
Manual equivalent:

Keep the install minimal.

```
sudo apt update
sudo apt full-upgrade
sudo apt install v4l-utils rsync curl
```

Do not install a desktop, `opencv`, Python ML tools, or a compiler toolchain. Builds happen on the PC.

### 3.4 Camera permissions

The user must be in the `video` group to open `/dev/video0`:

```
sudo usermod -aG video nico
```

Log out and back in, then check:

```
id                       # lists "video"
ls -l /dev/video*
```

### 3.5 Time

Set the correct time zone and keep the clock in sync, so PGN timestamps are right:

```
sudo timedatectl set-timezone Europe/Paris      # adjust
timedatectl status                              # "System clock synchronized: yes"
```

If it is not synchronized, `sudo apt install systemd-timesyncd` and retry.

### 3.6 BIOS settings

Set in the BIOS **(verify the options exist on this unit)**:

- Power on automatically after a power loss ("AC Power Recovery" or similar).
- Boot from the internal disk first. Disable booting from USB once the install is done.

### 3.7 Disk wear and logs (optional)

If storage is an SD card or small flash module, limit journal size:

```
sudo mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=50M\n' | sudo tee /etc/systemd/journald.conf.d/size.conf
sudo systemctl restart systemd-journald
```

## 4. Camera setup and verification

### 4.1 Format check

```
v4l2-ctl --list-formats-ext
```

Prefer `YUYV` at `640x480`.

### 4.2 Lock exposure and white balance

List the controls and note their exact names, as they differ between kernels and cameras:

```
v4l2-ctl --list-ctrls
```

Typical UVC control names **(verify against the actual output)**:

```
v4l2-ctl -d /dev/video0 --set-ctrl=auto_exposure=1                 # 1 = manual mode, 3 = automatic
v4l2-ctl -d /dev/video0 --set-ctrl=exposure_time_absolute=300      # pick a value from the lighting
v4l2-ctl -d /dev/video0 --set-ctrl=white_balance_automatic=0
v4l2-ctl -d /dev/video0 --set-ctrl=white_balance_temperature=4600
```

These settings are often reset when the camera is unplugged or the device closed.
NicoChess therefore applies them itself every time it opens the camera: auto white balance off, manual exposure, auto exposure priority off (`--no-lock-controls` disables this; `--exposure` and `--white-balance` set values).
This is implemented but **not yet tested on the C170**: check the `camera:` lines in the program's log to see which controls the camera accepted.

### 4.3 Test capture

```
v4l2-ctl -d /dev/video0 --set-fmt-video=width=640,height=480,pixelformat=YUYV \
  --stream-mmap --stream-count=1 --stream-to=/tmp/test.raw
```

Copy it to the PC and convert to an image to look at it (needs ffmpeg on the PC):

```
scp nico@<target-ip>:/tmp/test.raw .
ffmpeg -f rawvideo -pix_fmt yuyv422 -s 640x480 -i test.raw test.png
```

Check: sharp, board fills the frame, no flicker, no obvious glare.

### 4.4 Physical setup

Mount the tripod so the camera sees the whole board at about 45° tilt and about 20° lateral offset.
Use diffuse, constant light and avoid a window beside the board.
Once the position is chosen, do not move it. Any move means re-calibrating the 4 corners.

## 5. Build the program on the PC

Build on the PC. Do not build on the target.

The program is built in a Debian 12 i386 container, driven by `scripts/build-target.ps1` (Docker Desktop must be running):

```
.\scripts\build-target.ps1          # writes dist\nicochess
```

Why a container: the `v4l` camera crate generates its V4L2 bindings at build time (`bindgen`), which needs `libclang` and the Linux kernel headers. A Windows host does not have them, so a cross-compile with Zig and musl failed (`'linux/videodev2.h' file not found`). The container also has the same glibc as the target, so the binary runs there.
The first run builds the image (about 1 GB, a few minutes); later runs reuse it and the cargo cache. The build uses `--locked`. Details, and the other builds (Android, Windows), are in [ENVIRONMENT.md](ENVIRONMENT.md).

### 5.1 First proof

Copy `dist/nicochess` to the target (section 6.1) or use the USB key (section 10).
The 32-bit binary has already been run in a Debian 12 i386 container on the PC, with the demo source, so the build, the 32-bit runtime and the server are known to work. The real camera and the INEOS 600 have not been tried.

First tests on the target, in this order:

1. **No camera.** `./nicochess --source demo`, then open `http://<target-ip>:8080` from the PC and from a phone. Calibrate with the corners shown in the README (or press **Auto-detect the board**), start a game, and watch the demo play 33 moves. This tests the server, the page and the Atom's speed (watch `top` while it plays).
2. **The camera alone.** First `./nicochess --probe` (add `--device /dev/videoN` if needed): it lists the formats, frame sizes and controls the camera offers. Then `./nicochess` (source `camera`, device `/dev/video0`) and read the `camera:` lines in the output: the format chosen, and which controls were applied (white balance off, manual exposure, autofocus off). Then use the Calibrate tab and look at the picture. With the Yealink UVC30 Room, see the Webcam section of `docs/target-hardware.md`.
3. **A real game.** Set up the pieces, calibrate, press Start, and play a few moves. If moves are missed or wrong, see the tuning flags in `./nicochess --help` and the notes in `docs/requirements.md`.

## 6. Deploy and run

The simplest way is the USB key (section 10): `install.sh` installs the program, the user, the folders and the service in one go. With a network, `scripts/deploy.ps1` updates the program afterwards. The layout on the target is the same either way:

| What | Where |
|---|---|
| Program | `/opt/nicochess/nicochess` |
| Service | `nicochess.service` (systemd), run by the user `nicochess` (group `video`) |
| Settings | `/etc/default/nicochess`, one line `NICOCHESS_ARGS="..."`: `--width 640 --height 480 --fps 3` by default, `--source demo` after `install.sh --demo` |
| Games | `/var/lib/nicochess/games/` |
| Calibration and picture settings | `/var/lib/nicochess/calibration.json` and `/var/lib/nicochess/picture.json` |

### 6.1 Update the program

```
.\scripts\deploy.ps1 -Target nico@<target-ip>              # build in the container, copy, restart
.\scripts\deploy.ps1 -Target nico@<target-ip> -SkipBuild   # copy dist\nicochess as it is
```

The user on the target needs `sudo`, and `install.sh` must have been run once. By hand: `scp dist/nicochess nico@<target-ip>:/tmp/nicochess.new`, then `sudo install -m 755 /tmp/nicochess.new /opt/nicochess/nicochess && sudo systemctl restart nicochess`.

### 6.2 The service

`install.sh` writes `/etc/systemd/system/nicochess.service` (do not write it by hand):

```
[Unit]
Description=NicoChess board tracker

[Service]
User=nicochess
Group=nicochess
SupplementaryGroups=video
EnvironmentFile=-/etc/default/nicochess
ExecStart=/opt/nicochess/nicochess --listen 0.0.0.0:8080 --data-dir /var/lib/nicochess/games --calibration /var/lib/nicochess/calibration.json $NICOCHESS_ARGS
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

There is no `After=network-online.target`, so the service also starts on a machine with no network (Wi-Fi access point only).
To change the options, edit `/etc/default/nicochess` and run `sudo systemctl restart nicochess`. `/opt/nicochess/nicochess --help` lists them all. To test the camera by hand first, stop the service and run `/opt/nicochess/nicochess --device /dev/video0`.

```
systemctl status nicochess
journalctl -u nicochess -f
```

### 6.3 Use the page

From any machine on the network, open `http://<target-ip>:8080` in a browser.
The first page to connect is the controller and can start games and calibrate; the others only watch and replay (see `docs/requirements.md`).
There is no login. Keep the target on a trusted network (later, the Wi-Fi access point with a WPA2 passphrase) and do not expose the port to the Internet.

Games are JSON files in `/var/lib/nicochess/games`; the calibration and the picture settings are next to them in `/var/lib/nicochess`. Back up that folder to keep the games and the calibration.

## 7. Acceptance checklist

The target is ready for development when all of these are true:

- [ ] `lscpu` and `/proc/cpuinfo` results written down (see section 1).
- [ ] Debian 12 i386 installed without a desktop, boots to a login prompt.
- [ ] SSH key login works, password login disabled.
- [ ] The target keeps the same IP address after a reboot.
- [ ] `v4l2-ctl --list-formats-ext` shows `640x480` (YUYV preferred).
- [ ] Exposure and white balance can be locked, or the limitation is recorded.
- [ ] A test capture looks sharp and shows the whole board.
- [ ] The binary built by `scripts\build-target.ps1` runs on the target and opens the camera.
- [ ] The systemd service starts at boot and restarts after a crash.
- [ ] The page is reachable from the PC browser and from a phone.
- [ ] `./nicochess --source demo` plays the whole demo game without stalling the Atom.
- [ ] Calibration looks right on the real board (the green grid follows the squares).
- [ ] A few real moves are recognised correctly, and a wrong one can be fixed from the page.

## 8. Later: Wi-Fi access point for phone or tablet

Goal: a USB Wi-Fi dongle makes the target expose its own Wi-Fi network. A phone or tablet joins it and opens the HTTP page served by NicoChess to start a game and see the board.
This is done after the base install, and it is not needed for the first milestones.

Everything in this section is from memory and must be verified.

### 8.1 Choose the dongle

- The driver must support **access point (AP) mode**. On the target with the dongle plugged in:
  ```
  iw list | grep -A 12 "Supported interface modes"     # "AP" must be listed
  ```
- Atheros AR9271 (driver `ath9k_htc`) is the chipset I recall as most reliable for AP mode on Linux. 2.4 GHz only, which is enough for a phone.
- Careful with the TP-Link TL-WN722N: only version 1 uses that chip. Versions 2 and 3 use a Realtek chip.
- Many dongles need non-free firmware from Debian (for example `firmware-atheros`). Add the `non-free-firmware` component to `/etc/apt/sources.list` and install it while the target still has Ethernet.
- Check what is detected with `lsusb`, `dmesg | tail`, and `ip -br link`.

### 8.2 Software

```
sudo apt install hostapd dnsmasq iw
```

Network layout (example, to adapt):

| Item | Value |
|---|---|
| SSID | `NicoChess` |
| Security | WPA2 with a passphrase (8 characters or more) |
| Band and channel | 2.4 GHz, channel 6 |
| Country code | Set it (for example `FR`) |
| Target address on Wi-Fi | `192.168.4.1/24`, static |
| DHCP range for phones | `192.168.4.10` to `192.168.4.50` (`dnsmasq`) |
| Name | `nicochess.lan` points to `192.168.4.1` (`dnsmasq`) |

Steps:
1. Give the Wi-Fi interface a static address (in `/etc/network/interfaces` or a systemd-networkd file).
2. Write `/etc/hostapd/hostapd.conf` with the SSID, channel, country, WPA2 passphrase, and `hw_mode=g`.
3. Write a `dnsmasq` config limited to that interface (`interface=wlan0`, `dhcp-range=...`, `address=/nicochess.lan/192.168.4.1`).
4. Unmask and enable the services: `sudo systemctl unmask hostapd`, then `sudo systemctl enable --now hostapd dnsmasq`.
5. Make NicoChess listen on `0.0.0.0:80` (or 8080) and open `http://192.168.4.1` from a phone.

Keep the Ethernet interface as it is. Do not enable IP forwarding between the Wi-Fi and Ethernet: the phone only needs to reach the target.

### 8.3 Design implications

- Send the board state to the phone as JSON or SVG. Camera video is only streamed (Motion JPEG) to the page of the controller on the Calibrate tab, and the top-down board view only while that tab is open: it loads the N270 as little as possible, but keep the number of pages on the Calibrate tab small.
- A "start game", "set position" and "correct a square" interface on the phone replaces a local GUI.
- The page must work on a phone browser without Internet access (no external fonts, scripts or CDNs).

### 8.4 Security and policy

- Use a WPA2 passphrase, not an open network.
- Do not switch it on inside the corporate building without IT's agreement, as an unmanaged access point can be flagged as a rogue access point. Test and use it at home.

## 9. Alternative target: a laptop

If the target becomes a regular laptop, most of this plan is not needed: sections 1 to 4 (installing Debian i386 on the Atom), the i386 container build, and section 8 (Wi-Fi access point) become optional.
The reasoning is in `docs/requirements.md`, "Targets".

**Linux laptop**
1. Install the build tools: `sudo apt install build-essential clang libclang-dev linux-libc-dev pkg-config curl`, and Rust with `rustup`.
2. Build on the laptop: `cargo build --release` (native 64-bit, no container).
3. Check the camera: `./target/release/nicochess --probe --device /dev/videoN`.
4. Run it with the page on the laptop's own screen: `./target/release/nicochess --listen 127.0.0.1:8080 --open --width 1920 --height 1080 --fps 5`.
   The laptop's browser is the first to connect, so it is the controller. Use `--listen 0.0.0.0:8080` instead to let phones on the same network follow as viewers.
5. Optional: start it at login with a systemd user service or a desktop autostart entry, and put the browser in full screen (F11) or kiosk mode.

**Windows laptop**
- The laptop would run Linux, like the INEOS 600. The program also runs on Windows, camera included (Media Foundation, added 2026-11-12): `cargo run --release -- --probe` lists the cameras, then `cargo run --release -- --device 0 --width 1280 --height 720 --open` uses one.
  `cargo run --release -- --source demo --open` plays the demo game in the local browser, without a camera.
  If the page does not open on Windows with "forbidden by its access permissions", the port is reserved (Docker Desktop and Hyper-V reserve ranges that include 8080): check with `netsh int ipv4 show excludedportrange protocol=tcp` and pass another port. The reserved ranges change from one start of Docker to the next (18080 worked one day and not the next), so look at the list and pick a port outside it, e.g. `--listen 127.0.0.1:28080`.

**Both**
- Raise the resolution and the frame rate: the Atom limits (640x480, 3 fps, no heavy decoding) no longer apply.
- Engine analysis becomes comfortable on a laptop (see "Engine analysis with Stockfish" in `docs/requirements.md`).

## 10. The USB key: everything in one folder

`scripts/make-usb-bundle.ps1` prepares the folder `usb\` (git-ignored, about 71 MB), to copy onto a FAT32 USB key:

| In the folder | What it is |
|---|---|
| `nicochess` | The 32-bit program, built in the container |
| `install.sh` | Installs the packages from the key (no network), the program in `/opt/nicochess`, the user `nicochess`, and the service; writes a report of the machine |
| `debs/` | 141 Debian 12 i386 packages with their dependencies, as a small offline apt repository (includes the Wi-Fi firmware) |
| `setup-ap.sh` | Wi-Fi access point with `hostapd` and `dnsmasq`, on its own systemd service |
| `target-setup.sh` | Report on the machine (CPU flags, memory, disk, camera formats, Wi-Fi) |
| `README.txt` | The steps, on the key itself |
| `docs/`, `SHA256SUMS` | Reference documents, and checksums that `install.sh` verifies |

```
.\scripts\make-usb-bundle.ps1          # build everything (Docker Desktop must be running)
```

Then copy the **content** of `usb\` to the root of the key. On the target: `su -`, mount the key, `bash /mnt/install.sh --demo`.
Use `--demo` first to try the page without the camera, then edit `/etc/default/nicochess` to use the camera.

What was tested, and what was not:

- **Tested:** the installation from the key in a fresh Debian 12 i386 container **with the network disabled**. The packages (and the Wi-Fi firmware) install, the user is created, the program is installed and runs.
  The access point script was run with `--dry-run` and its files read.
- **Not tested:** starting the service under a real systemd, the access point on a real dongle, and everything with the real camera. Each of these is the first thing to check on the target.
- The key replaces section 3.3 (packages) and section 6 (deploy and service) for a machine with no network. With a network, `scripts/deploy.ps1 -Target user@host` copies a new build and restarts the service.

## 11. Testing the camera on this PC, before the target

Docker Desktop does not hand USB devices to containers by itself, so a webcam plugged into the PC is not visible in the container.
It can be done with `usbipd-win`, which hands a USB device to WSL (the machine Docker Desktop runs in).
On this PC the WSL kernel (6.18) already contains the webcam driver (`uvcvideo`) and the USB/IP client as loadable modules, so only `usbipd-win` is missing.

```
winget install --exact dorssel.usbipd-win     (administrator PowerShell, once)
usbipd list                                    find the webcam, note its BUSID (for example 2-4)
usbipd bind --busid 2-4                        (administrator, once per device)
usbipd attach --wsl --busid 2-4                hand it to WSL
wsl                                            then, in the WSL window:   sudo modprobe uvcvideo ; ls /dev/video*
```

**Administrator rights are needed** for `winget install` and for `usbipd bind` (once per device). Check that the console really is elevated: `whoami` must print the administrator account, and the title bar says "Administrator".
On a company-managed PC where your own account is not a local administrator, binding a USB device (it installs a driver) is for IT to approve. `usbipd attach` itself should then work without administrator rights (not verified).
If that is not possible, skip this section: the same camera checks (`--probe`, calibration, a real game) are done on the target itself.

Then, from the project folder:

```
.\scripts\test-camera-docker.ps1 -Probe        formats, sizes and controls of the camera
.\scripts\test-camera-docker.ps1               the program with the camera, page on http://localhost:8080
```

What this tests: the real V4L2 code (format choice, YUYV, MJPEG or NV12 conversion, the exposure and focus controls), calibration on a real picture,
and the move detection if you put a real board in front of the camera. It runs the same 32-bit program as the target.

Limits and cautions:

- **The built-in webcam of a laptop has a different lens, resolution and controls than the C170 or the Yealink.** It proves the code works, not that the target camera is good enough.
  The same steps work for the C170 or the Yealink plugged into the PC.
- **A 32-bit program on this PC's 64-bit kernel** goes through the kernel's compatibility layer for the camera calls. The Atom has a 32-bit kernel, which does not. If it works here it very likely works there, but the reverse is not guaranteed.
- While the camera is attached to WSL, Windows programs (Teams, the Camera app) cannot use it. Give it back with `usbipd detach --busid 2-4`.
- The laptop may expose more than one camera (an infrared one for Windows Hello). Pick the one that shows a normal picture.
- Not covered: the systemd service and the access point (a container has no systemd), and the speed of the Atom.
- Games recorded in the container are lost when it stops (no volume is mounted). That is fine for a test.

## Open questions

- Actual RAM and storage of the unit (not yet known).
- Whether the BIOS allows booting from USB and has an AC power recovery option.
- Whether the C170 exposes manual exposure and white balance on Linux.
- Exact Debian 12 support dates for i386.
- Which Wi-Fi dongle supports AP mode with a Debian 12 driver, and whether it needs non-free firmware.
- Whether the corporate network allows the target to connect, otherwise the install is done at home.
