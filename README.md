# FlashTester

Website: [flashtester.eu](https://flashtester.eu/).

![FlashTester after a 1 GB full test of a generic 64 GB USB drive: verdict OK, block map and speed graph](website/screenshot-result.png)

Finds out whether a USB stick, SD card or other disk really stores what it claims. FlashTester does what [h2testw](https://www.heise.de/download/product/h2testw-50539) does and adds the things h2testw can't:

- **Quick capacity probe:** finds a fake drive's real capacity in minutes, not hours.
- **Seeded test data:** every run writes different data, so a drive that silently stopped accepting writes can't pass on a previous run's data.
- **Early abort:** while filling a drive, earlier data is re-checked; a fake is caught as soon as it starts overwriting itself.
- **Raw mode** (whole disk) as well as file mode (free space only, keeps your files).
- **Speed map, stalls, sequential benchmark, random 4K IOPS, many-small-files test.**
- **GUI:** pick a drive and a test, watch the block map fill in (the drive as a grid of cells: untested, written, verified, corrupted, aliased, unreadable, with offsets, speed and errors on hover) next to a speed-by-position graph, and read a plain verdict with the next step.
- **Trust checks:** unplug/replug before verifying, identity changes across replug, placeholder serials, USB 2 link warning, pull-the-plug-mid-write test, retention check later.
- **Reports:** a plain verdict, a detailed HTML report, JSON, exit codes, and a one-page evidence sheet for refund claims.
- **Fix:** repartition a fake drive to the capacity that really works.

[![CI](https://github.com/Dissimilis/FlashTester/actions/workflows/release.yml/badge.svg?branch=main)](https://github.com/Dissimilis/FlashTester/actions/workflows/release.yml)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20FreeBSD-lightgrey.svg)](#download-and-run)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-donate-yellow.svg)](https://buymeacoffee.com/dissimilis)

## The GUI

The drive list on the left offers each volume (file mode: tests free space, keeps your files) and each whole disk (raw mode: tests everything, erases it). Pick one, pick a test, and the panel on the right explains what the test does and what it will touch. Whole-disk tests are shown in red and need a confirmation tick. Tests that need administrator rights say so and offer to restart FlashTester elevated. While a test runs, the block map and speed graph fill in; at the end you get the verdict, the next sensible step and buttons for the reports.

![Setting up a full test on a volume](website/screenshot-setup.png)

## Download and run

One self-contained file per platform: no installer, no dependencies. Started without arguments it opens the GUI (where there is one); with arguments it runs on the command line.

| Platform | File | Interface |
|---|---|---|
| Windows 10 / 11, 64-bit | `flashtester.exe` | GUI and command line |
| Linux x86_64, any distribution | `flashtester-linux-x86_64` | GUI (X11 or XWayland) and command line |
| Linux aarch64 (64-bit ARM: Raspberry Pi with a 64-bit OS, ARM boards, routers) | `flashtester-linux-aarch64` | GUI (X11 or XWayland) and command line |
| FreeBSD 13.5 to 16.0, amd64 | `flashtester-freebsd-x86_64` | command line |

### Windows

- Run `flashtester.exe`. Volumes go by drive letter (`flashtester test E:`), whole disks by the number `flashtester list` shows (`--disk 3`).
- File-mode tests run as a normal user. Raw-mode tests and the free-space probe need an administrator terminal; the GUI offers to restart elevated.
- Windows is kept awake while a test runs.
- The GUI needs OpenGL. Where there is none (for example a remote desktop without a GPU), use the command line.

### Linux

```sh
chmod +x flashtester-linux-x86_64
./flashtester-linux-x86_64                  # GUI, inside an X11 or Wayland (XWayland) desktop
./flashtester-linux-x86_64 test /media/you/STICK
sudo ./flashtester-linux-x86_64 probe --disk sdb --destroy
```

- Volumes go by mount point, whole disks by kernel name (`--disk sdb`, `--disk nvme1n1`, `--disk mmcblk1`).
- Raw-mode tests and the free-space probe need root, and raw writes need the disk unmounted. In the GUI, "Restart as root" goes through `pkexec`.
- `fix` uses `sfdisk` and `mkfs.*` (`--fs exfat`, `fat32`, `ntfs` or `ext4`).
- The free-space probe needs a file system with `fallocate` and `FIEMAP`, such as ext4 or xfs.

### OpenWrt

- Use `flashtester-linux-aarch64` on most current routers and `flashtester-linux-x86_64` on x86 builds. Routers with MIPS or 32-bit ARM CPUs are not supported.
- A USB stick only appears as a disk once its driver is installed: `kmod-usb-storage`, and for most USB 3 sticks also `kmod-usb-storage-uas` (without it, a UAS stick shows up on the USB bus but never becomes `/dev/sdX`).
- `fix` needs `sfdisk` and a formatter: `exfat-mkfs` for exFAT, `e2fsprogs` for ext4.
- Install packages with `apk add` (OpenWrt 25.12 and later) or `opkg install` (older releases).

### FreeBSD

- Command line only. Built against FreeBSD 14.5; runs on 13.5 to 16.0.
- Volumes go by mount point (UFS, FAT and ZFS), whole disks by device name (`--disk da0`, `--disk ada1`, `--disk nda0`).
- Raw-mode tests need root. FreeBSD refuses raw writes while anything on the disk is mounted, used as swap, or held by ZFS or another GEOM layer (mirror, geli).
- `fix` uses `gpart` with `newfs_msdos` (`--fs fat32`, the default) or `newfs` (`--fs ufs`); the base system has no exFAT or NTFS formatter.
- No free-space probe: FreeBSD cannot report where a file's blocks are on the disk. Use the raw probe on an empty drive, or a full test.

## Safety

Any disk can be tested (USB sticks, card readers, internal SATA/NVMe drives, VHDs) **except the disk the running OS uses**: on Windows a disk holding the Windows folder, the boot partition or a page file; on Linux any disk under `/`, `/boot`, `/boot/efi`, `/usr` or `/var` (followed through LVM, RAID and device-mapper layers) or holding swap; on FreeBSD the same mount points, swap and the crash-dump device, followed through partitions, labels, geli, gmirror, ZFS pools and file-backed memory disks. It is refused, always, with no override. Network shares and FUSE pools such as mergerfs are refused because they can't be mapped to one disk. Whole-disk tests destroy everything on the disk and need `--destroy` (or the confirmation tick box in the GUI) and an administrator terminal. File-mode tests only use free space.

## Quick start

```text
flashtester list                       # disks, identity, link speed, what may be tested
flashtester test E:                    # fill free space, verify (like h2testw)
flashtester probe --disk 3 --destroy   # real capacity in minutes (admin, destroys data)
flashtester fix --disk 3 --destroy     # probe, then partition+format to the real size
```

On Linux use a mount point and a kernel name (`test /media/you/STICK`, `--disk sdb`), on FreeBSD a device name (`--disk da0`).

## Commands

| Command | What it does | Admin |
|---|---|---|
| `list` | Disks and volumes, protection status, USB ID and link speed | no |
| `info E:` / `info --disk N` | Vendor, product, serial, USB VID:PID, link, volumes, suspicious identity | no |
| `test E:` | Fill free space with seeded data, read it all back. `--size 8G`, `--replug`, `--passes N`, `--cleanup` | no |
| `test --disk N --destroy` | Same over the whole disk (raw) | yes |
| `endurance E: [--passes N]` | Write + verify again and again with new seeds (0 = until error / Ctrl+C) | raw only |
| `verify E:` | Re-read data from an earlier test (retention). `--loops N` (0 = endless), `--h2w` for h2testw files | raw only |
| `probe --disk N --destroy` | Quick fake-capacity probe on the raw disk | yes |
| `probe E:` | Quick probe inside free space (files are created, mapped to disk positions, and deleted) | yes |
| `scan --disk N` | Read-only surface scan: unreadable and slow regions | yes |
| `iops E:` / `iops --disk N` | Random 4K at queue depth 1 and 32 (raw writes need `--write --destroy`) | raw only |
| `bench E: [--size 1G]` / `bench --disk N` | Sequential write+read speed with a speed graph (speed only, not a capacity check; raw is read-only unless `--write --destroy`) | raw only |
| `small-files E: [--count N]` | Create, rename, delete and verify thousands of small files | no |
| `plug-pull E:` | Guided: pull the drive out while it writes; flushed data must survive | no |
| `fix --disk N --destroy` | Probe, then one partition of the real capacity (`--size 7.5G` / `full`, `--fs exfat|fat32|ntfs`) | yes |
| `clean E:` | Delete FlashTester's test data | no |
| `render result.json --html r.html` | Turn a saved JSON result into an HTML report / `--evidence-out` sheet | no |

Options for every command: `--json` (result on stdout), `--report file.html|file.json`, `--evidence file.html`, `-q` (no progress).

**Exit codes:** 0 OK, 1 problem found (fake, defective, read-only, unreliable), 2 refused (system disk, missing `--destroy`, not admin), 3 error, 130 cancelled.

## Verdicts

- **OK:** everything written came back intact.
- **FAKE:** less capacity than claimed. Data written past the real end is lost, or wraps around and overwrites the start of the drive.
- **DEFECTIVE:** data lost or corrupted inside the capacity.
- **READ-ONLY:** the drive accepts writes but keeps its old data.
- **UNRELIABLE:** the data verified, but there were disconnects, read errors or unstable reads. Try another port, cable or reader.

Each lost 512-byte sector is classified like h2testw does (*slightly changed* under 8 bits, *corrupted*, *aliased* = holds another position's data), plus *stale* (holds data from an earlier run: the write never landed) and *unreadable*. Bad chunks are re-read: the same wrong bytes each time point at the flash; different bytes or errors that clear on retry point at the connection.

## How the quick probe works

Cheap fakes ignore the high address bits, so writes past the real capacity wrap around. The probe writes 4 KB blocks at about 1000 positions on a power-of-two grid across the claimed capacity, once in ascending and once in descending order, each time with a new random tag, and reads them back. A wrapped write lands exactly on another probe, which then holds the wrong position's data. That gives the real size directly. Fakes that drop writes or return errors past their end show a tail of failed probes, and the boundary is narrowed down with finer grids. Anything stranger is caught by the full test.

## Limits

- File mode tests free space only. Data already on the drive isn't checked, and on a fake drive filling free space can overwrite existing files (the fake wraps onto them). Back up first.
- Software only sees errors the controller's error correction couldn't fix. There is no health or remaining-life percentage, by design.
- Repartitioning a fake drive to its real size makes it usable; it doesn't make it trustworthy.
- FreeBSD has no free-space probe (see FreeBSD above).

## Tested

- **Windows 11:** USB sticks, an NVMe SSD in a USB enclosure, and throwaway VHDs (`test\vhd-test.ps1`).
- **Linux on hardware:** Debian 13 (OpenMediaVault) and Fedora 44 on x86_64; Armbian on ODROID C2 and N2+ and Ubuntu 26.04 on AWS Graviton4, all aarch64.
- **Linux in QEMU/KVM** with a real USB stick passed through, installed or booted live from a USB stick: AlmaLinux 10, Alpine 3.24, Arch, Bazzite, Chimera, Debian (on RAID, LUKS and LVM), EndeavourOS, Gentoo, Guix System 1.5, Kali 2026.2, Linux Mint 22, Manjaro, NixOS 26.05, OpenWrt (x86), openSUSE Tumbleweed, Rocky Linux 10, Slackware, SystemRescue, Tiny Core 17, Ubuntu 22.04 and 26.04, Void (glibc and musl) and Zorin OS 18. Kernels 5.15 to 7.2.
- **Docker:** works with `docker run --privileged -v /dev:/dev`. Without the host's `/dev`, FlashTester can't tell which disks the host uses and refuses all of them.
- **FreeBSD** in QEMU/KVM: 13.5, 14.4, 14.5, 15.0, 15.1 and 16.0-CURRENT.
- Fake drives (wraparound, dropped writes, read-only) are covered by unit tests on a simulated drive.

## Building

Rust with the GNU toolchain (no Visual Studio needed):

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
cargo test --release
cargo build --release                          # target\release\flashtester.exe
# Static Linux binaries from any host (Rust's bundled lld, see .cargo/config.toml):
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
# Static FreeBSD binary: the same lld, linked against FreeBSD's own startup files and
# static libraries. Unpack them from FreeBSD 14.5's base.txz into target/freebsd-sysroot
# and strip their debug sections first, as .github/workflows/release.yml does.
rustup target add x86_64-unknown-freebsd
cargo build --release --target x86_64-unknown-freebsd
powershell -File test\check-imports.ps1        # only Windows DLLs imported
powershell -ExecutionPolicy Bypass -File test\vhd-test.ps1   # elevated; uses a throwaway VHD
```

## Licence

MIT. The GUI embeds the Ubuntu font (Regular and Medium; Copyright 2011 Canonical Ltd.), which is under the [Ubuntu Font Licence 1.0](assets/fonts/UFL.txt).
