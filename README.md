# FlashTester

Website: [flashtester.eu](https://flashtester.eu/).

![FlashTester after a 4 GB full test of a Verbatim Store 'n' Go USB stick: verdict OK, block map and speed graph](website/screenshot-result.png)

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

Windows is kept awake while a test runs. One file, `flashtester.exe`, no installer. Run it without arguments for the GUI, with arguments for the command line. Windows 10/11, 64-bit.

In the GUI, the drive list on the left offers each volume (file mode: tests free space, keeps your files) and the whole disk (raw mode: tests everything, erases it). Whole-disk tests are shown in red and need a confirmation tick. Tests that need administrator rights say so and offer to restart FlashTester elevated.

![Setting up a full test on a volume](website/screenshot-setup.png)

**Linux:** GUI and command line, as one static file (`flashtester-linux-x86_64` or `flashtester-linux-aarch64`; no dependencies, any distribution). Started without arguments inside an X11 desktop, or a Wayland desktop with XWayland, it opens the same GUI; "Restart as root" goes through `pkexec`. Elsewhere it runs as a command-line tool. Disks go by their kernel names (`--disk sdb`, `--disk nvme1n1`, `--disk mmcblk1`); file mode takes a mount point (`flashtester test /mnt/usb`). Raw-disk commands need root, and a disk must be unmounted before raw writes. `fix` uses `sfdisk` and `mkfs.*` (`--fs ext4` is also available); the free-space probe needs a file system with `fallocate` and `FIEMAP`, such as ext4 or xfs.

**OpenWrt:** use the static binary for the router's CPU (`flashtester-linux-aarch64` on most current routers, `-x86_64` on x86 builds). A USB stick only appears as a disk once its driver is installed: `kmod-usb-storage`, and for most USB 3 sticks also `kmod-usb-storage-uas` (without it a UAS stick is detected on the USB bus but never becomes `/dev/sdX`). `fix` needs `sfdisk` and a formatter (`exfat-mkfs` for exFAT, `e2fsprogs` for ext4). Install them with `apk add` (OpenWrt 25.12 and later) or `opkg install` (older releases).

**FreeBSD:** command line only, as one static file (`flashtester-freebsd-x86_64`, amd64; built against FreeBSD 14.5, runs on 13.5 to 16.0). Disks go by their device names (`--disk da0`, `--disk ada1`, `--disk nda0`); file mode takes a mount point and works on UFS, FAT and ZFS. Raw-disk commands need root, and FreeBSD refuses raw writes while anything on the disk is mounted, used as swap, or held by ZFS or another GEOM layer (mirror, geli). `fix` uses `gpart` with `newfs_msdos` (`--fs fat32`, the default) or `newfs` (`--fs ufs`): the base system has no exFAT or NTFS formatter. The free-space probe is not available, because FreeBSD cannot report where a file's blocks are on the disk.

## Safety

Any disk can be tested (USB sticks, card readers, internal SATA/NVMe drives, VHDs) **except the disk the running OS uses**: on Windows a disk holding the Windows folder, the boot partition or a page file; on Linux any disk under `/`, `/boot`, `/boot/efi`, `/usr` or `/var` (followed through LVM, RAID and device-mapper layers) or holding swap; on FreeBSD the same mount points, swap and the crash-dump device, followed through partitions, labels, geli, gmirror, ZFS pools and file-backed memory disks. It is refused, always, with no override. Network shares and FUSE pools such as mergerfs are refused because they can't be mapped to one disk. Whole-disk tests destroy everything on the disk and need `--destroy` (or the confirmation tick box in the GUI) and an administrator terminal. File-mode tests only use free space.

## Quick start

```text
flashtester list                       # disks, identity, link speed, what may be tested
flashtester test E:                    # fill free space, verify (like h2testw)
flashtester probe --disk 3 --destroy   # real capacity in minutes (admin, destroys data)
flashtester fix --disk 3 --destroy     # probe, then partition+format to the real size
```

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
- FreeBSD has no free-space probe (see above); use the raw probe on an empty drive, or a full test.

## Tested

On Windows 11 with a "128 GB" ADATA USB stick (VID 24A9, no vendor name, placeholder serial; genuine 126.66 GB):
full file-mode test of all 126.66 GB (48 min write at 44 MB/s, 16 min read at 131 MB/s, 0 errors); raw probe in 8 s and free-space probe in 12 s, both genuine;
raw 4 GB test and verify; surface scan of the whole disk; sequential bench; random 4K IOPS; small files; `fix`;
unplug/replug (device disabled and re-enabled between write and verify); pull-the-plug (USB port power-cycled mid-write: all 805 MB confirmed by a flush survived, the 84 MB in flight was reported, not counted as a defect).
On a 512 GB NVMe SSD in a Realtek RTL9210C USB enclosure (a fixed, non-removable disk like an internal drive): file-mode test, verify, bench, IOPS and small files; then raw probe (1 s), free-space probe, 16 GB raw test and verify, endurance, raw bench (332/398 MB/s write/read), random 4K writes, a surface scan of all 512 GB at 406 MB/s and `fix`. FlashTester flagged that the enclosure supports 10 Gbit/s but ran at 5 Gbit/s, and caught one run with unstable reads and 19 s stalls (a flaky link; the rerun was clean).
The GUI was driven through Windows UI Automation (AccessKit): select a drive and a test, set a size limit, start it, tick the confirmation for a whole-disk test, read the verdict. The screenshots above come from a real 4 GB file-mode test of a Verbatim Store 'n' Go 64 GB stick.
`test\vhd-test.ps1` runs 21 checks against a throwaway VHD, a fixed (non-removable) disk standing in for an internal drive, including the system-disk and junction guards. The machine's only internal disk holds Windows and is refused, as it must be.
Fake-drive behaviour (wraparound, dropped writes, read-only) is covered by unit tests on a simulated drive.

Linux, with the static binaries on five machines:
- **OpenMediaVault NAS (Debian 13, x86_64):** non-destructive tests of the internal Verbatim SSD (btrfs) and a 4 TB USB HDD (exFAT) in a live backup pool: 2 GB file tests, verify, bench, IOPS, small files, 20 GB read-only scans. Full destructive suites on two USB sticks (probe, raw test, endurance, bench, random writes, scan, `fix`, file mode on ext4 including the free-space probe), each restored to its original file system afterwards. The system disk, `/` and the mergerfs pool were refused.
- **Fedora 44 (x86_64), 1.92 TB Samsung SATA SSD in a 10 Gbit/s USB enclosure:** full destructive suite (raw 401/546 MB/s write/read), file mode on ext4, free-space probe across 1.92 TB in 10 s.
- **ODROID C2 (Armbian, aarch64, USB 2.0), 125 GB generic USB stick:** full destructive suite, file mode on ext4 including the free-space probe, raw and file-mode replug, and pull-the-plug (USB de-authorized mid-write: all 134 MB confirmed by a flush survived). The SD card holding `/` was refused.
- **ODROID N2+ (Armbian, aarch64):** identity, the system-disk guard (the SD card holding `/` is refused) and a raw probe of a Kingston DataTraveler 3.0 (genuine 31.00 GB). The stick then dropped off the USB bus during the raw write and did not re-enumerate; the full aarch64 suite ran on AWS instead.
- **AWS Graviton4 (Ubuntu 26.04, aarch64), temporary 2 GB EBS volume:** full destructive suite, file mode on ext4 including the free-space probe, and replug and pull-the-plug tests using real EBS detach/attach (a forced detach mid-write: all 1.74 GB confirmed by a flush survived). The boot volume was refused.

The Linux GUI was checked on Fedora 44 under Xvfb, showing every view with simulated drives; it has not yet been driven through a real test on Linux.

FreeBSD, with the static binary in QEMU/KVM virtual machines, one per release: 13.5, 14.4, 14.5, 15.0, 15.1 and a 16.0-CURRENT snapshot (2026-10-12). Each ran the unit tests and the same suite on an emulated 2 GB USB stick and a second virtual disk: identity (USB IDs, serial, link speed), the system-disk guard, raw probe, 128 MB raw test, `fix` to FAT32 and to UFS, file-mode test and small files, refusal of a raw write while the stick was mounted, file mode on a ZFS pool, and swap on a file inside that pool, which made the pool's disk protected. All passed on every release; on the 16.0 snapshot the ZFS steps were skipped because its ZFS module didn't load on its own kernel. On 15.1 also: verify, surface scan, bench, random 4K IOPS, nullfs mounts (followed to the disk underneath; a nullfs view of the system disk is refused) and a file system on a file-backed memory disk (refused, like a Linux loop device). Not yet tested on FreeBSD hardware with a real stick.

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

MIT
