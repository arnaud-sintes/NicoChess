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
- A wired network connection for the target (Ethernet, DHCP). Wi-Fi is not planned.
- A keyboard and a monitor for the install only. After that the target is headless.
- The Logitech C170 and a tripod.
- Rufus or balenaEtcher on the PC to write the USB stick.

## 0. Already downloaded on the PC

Everything below is in `downloads/` (git-ignored). Checksums were verified on 2026-11-03.

| What | Where | Check done |
|---|---|---|
| Debian 12.15.0 i386 netinst ISO (656 MB) | `downloads/debian-12.15.0-i386/debian-12.15.0-i386-netinst.iso` | SHA256 matches `SHA256SUMS`, and the `SHA256SUMS.sign` signature is good for the Debian CD signing key (fingerprint `DF9B 9C49 EAA9 2984 3258 9D76 DA87 E80D 6294 BE9B`) |
| Rufus 4.15 portable | `downloads/rufus-4.15/rufus-4.15.exe` | Not verified (a `.sig` file is next to it) |
| Zig 0.17.0 (Windows, portable) | `downloads/tools/zig-x86_64-windows-0.17.0/zig.exe` | SHA256 matches the Zig download index |
| `cargo-zigbuild` 0.24.0 | `downloads/tools/cargo-zigbuild/cargo-zigbuild.exe` | SHA256 matches the release `.sha256` file |
| Rust std for `i686-unknown-linux-musl` | installed in the PC's rustup toolchain | `rustup target list --installed` |

Proof that the toolchain works: a throwaway "hello" program built with `cargo zigbuild --release --target i686-unknown-linux-musl` gave a static 32-bit ELF (Intel i386). It was **not** run on the target yet.

Not downloaded:
- `ffmpeg`, used in step 4.3 to view a raw test capture on the PC. Install it with `winget install ffmpeg` or use WSL (`sudo apt install ffmpeg`).
- The `i386/debian:12` Docker image, only needed if the musl build fails (step 5.2).
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

Record the results in a new file `docs/target-findings.md`.

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
The NicoChess program should therefore apply them itself every time it opens the camera (`nokhwa` exposes camera controls) **(verify)**.
This is a design requirement to add to `docs/design.md` once confirmed.

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

## 5. Build toolchain on the PC

Cross-compile from the PC. Do not build on the target.

### 5.0 Use the portable tools

`zig.exe` and `cargo-zigbuild.exe` are in `downloads/tools/` and are not on the PATH.
In PowerShell, from the project root, for the current session:

```
$env:PATH = "$PWD\downloads\tools\zig-x86_64-windows-0.17.0;$PWD\downloads\tools\cargo-zigbuild;$env:PATH"
cargo zigbuild --release --target i686-unknown-linux-musl
```

Zig 0.17.0 with `cargo-zigbuild` 0.24.0 worked for the hello test. Pin both versions if a later update breaks the build.

### 5.1 Target choice

Primary: **`i686-unknown-linux-musl`**, a fully static binary.
It avoids glibc version mismatches between the PC's build environment and Debian 12 (glibc 2.36) **(verify)**.

```
rustup target add i686-unknown-linux-musl
```

### 5.2 Linker on Windows

Rust needs a Linux-capable linker. Options, in order of preference **(verify each on your PC)**:

1. **`cargo-zigbuild`**: uses Zig as the C compiler and linker.
   ```
   cargo install cargo-zigbuild
   cargo zigbuild --release --target i686-unknown-linux-musl
   ```
   Zig must be installed and on the PATH.
2. **WSL2 with Debian**: install the Rust toolchain inside WSL and build there. Simplest if `cargo-zigbuild` fails on this project's dependencies.
3. **Docker with an `i386/debian:12` image**: builds in the same environment as the target, which gives the best binary compatibility. Slower to set up.

If the camera crate (`nokhwa`, via the `v4l` crate) pulls in C libraries that do not cross-compile with musl, fall back to option 3 and the `i686-unknown-linux-gnu` target **(verify)**.

### 5.3 First proof

Before any chess code, cross-compile a trivial "hello" program and a minimal `nokhwa` capture program, copy them to the target and run them.
This confirms the toolchain, the camera crate and the 32-bit target all work together, which is the main technical risk.

## 6. Deploy and run

### 6.1 Copy the binary

```
scp target/i686-unknown-linux-musl/release/nicochess nico@<target-ip>:~/nicochess
```

Or with `rsync`, which is faster for repeated deploys:

```
rsync -avz --progress target/i686-unknown-linux-musl/release/nicochess nico@<target-ip>:~/
```

### 6.2 Run as a service

Create `/etc/systemd/system/nicochess.service` on the target:

```
[Unit]
Description=NicoChess board tracker
After=network-online.target
Wants=network-online.target

[Service]
User=nico
Group=nico
SupplementaryGroups=video
WorkingDirectory=/home/nico
ExecStart=/home/nico/nicochess --listen 0.0.0.0:8080
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

The `--listen` flag is a placeholder until the program exists.

Enable and start it:

```
sudo systemctl daemon-reload
sudo systemctl enable --now nicochess
systemctl status nicochess
journalctl -u nicochess -f
```

### 6.3 View the preview

From any machine on the network, open `http://<target-ip>:8080` in a browser.
There is no authentication in the plan. Keep the target on a trusted home network and do not expose the port to the Internet.

## 7. Acceptance checklist

The target is ready for development when all of these are true:

- [ ] `lscpu` and `/proc/cpuinfo` results recorded in `docs/target-findings.md`.
- [ ] Debian 12 i386 installed without a desktop, boots to a login prompt.
- [ ] SSH key login works, password login disabled.
- [ ] The target keeps the same IP address after a reboot.
- [ ] `v4l2-ctl --list-formats-ext` shows `640x480` (YUYV preferred).
- [ ] Exposure and white balance can be locked, or the limitation is recorded.
- [ ] A test capture looks sharp and shows the whole board.
- [ ] A cross-compiled binary runs on the target and opens the camera.
- [ ] The systemd service starts at boot and restarts after a crash.
- [ ] The preview is reachable from the PC browser.

## Open questions

- Actual RAM and storage of the unit (not yet known).
- Whether the BIOS allows booting from USB and has an AC power recovery option.
- Whether the C170 exposes manual exposure and white balance on Linux.
- Which cross-compile route (`cargo-zigbuild`, WSL2, Docker) works with the chosen crates.
- Exact Debian 12 support dates for i386.
