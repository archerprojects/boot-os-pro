# Boot OS Pro

**Version 1.0.0** · Tauri 2 · Rust · React 18 · Linux x86_64 · GPLv3

Boot OS Pro is a desktop application for creating multiboot USB connected drives that boot via GRUB2 on both EFI and legacy BIOS machines. It ships with Lean Linux and runs on any Ubuntu 24.04+, Linux Mint 22+, or Debian 13+ system.

```
Developed for Lean Linux by:
Developer:  archerprojects
Contact:    archer.projects@proton.me
Maintainer: archerprojects <archer.projects@proton.me>
archerprojects (archer.projects@proton.me)
https://github.com/archerprojects/boot-os-pro
```

---

## What it does

Boot OS Pro writes a structured, self-describing multiboot drive. Each ISO you add is either:

- **Persistent** — the OS squashfs is extracted into its own dedicated ext4 partition. Boots directly by label, fully writable, changes survive reboot. Universal persistence — no casper-rw overlay, no distro-specific hooks, works across Ubuntu/Mint/Debian/Fedora/Arch families.
- **Live session** — the ISO file is copied to a shared FAT32 free space partition and loopback-booted by GRUB. No persistence. Multiple live ISOs can coexist on one drive, added additively without reformatting.

---

## Screenshots

| | | |
|---|---|---|
| ![Select drive](screenshots/select-drive.jpg) | ![Configure drive](screenshots/configure-drive.jpg) | ![Disk Manager](screenshots/disk-manager.jpg) |
| Select Drive | Configure | Disk Manager |

---

## Drive layout

```
[ BIOS Boot 1MiB ][ EFI/ESP 512MiB FAT32 ][ BOOTOSPRO 40MiB FAT32 ][ ISO1 ext4 ][ ISO2 ext4 ][ FREESPACE FAT32 ]
```

| Partition | Purpose |
|---|---|
| BIOS Boot | GRUB i386-pc core for legacy BIOS boot on GPT |
| EFI (ESP) | GRUB EFI bootloader + grub.cfg + manifest.json |
| BOOTOSPRO | 40 MiB FAT32 installer payload — mounts on Linux, Windows, macOS |
| ISO partitions | One ext4 per persistent OS, labelled per ISO |
| FREESPACE | Shared FAT32 partition holding live-session ISOs as files |

The free space partition is optional — omitted when the drive is filled with persistent partitions.

---

## Manifest

A JSON file at `/bootospro/manifest.json` on the EFI partition is the master record of everything on the drive: persistent OS partitions, live ISOs, free space, installer payload, and per-partition filesystem UUIDs.

GRUB is regenerated as a pure function of the manifest on every change, so the boot menu and drive reality never drift. On drive selection, the app reads the manifest and compares its recorded partition UUIDs against live `blkid` output — a mismatch signals that a partition was reformatted outside the app (e.g. with GParted).

**Drift handling:** when a mismatch is detected the app blocks further configuration of that drive and offers **Rebuild from disk** — a one-click reconciliation that re-probes the physical partitions, rebuilds the manifest from reality, and rewrites GRUB to match. Reconciliation is rebuilt from bare partitions, so display names and boot parameters it cannot probe come back as best-effort defaults. Persistent partitions boot by filesystem UUID (label fallback for pre-UUID records), so entries stay correct even when another attached drive carries the same label.

The manifest is treated as untrusted input. All values that reach privileged commands are sanitised on read (labels, filenames, boot params, paths).

---

## Application structure

```
bootospro-app-V1_1/
├── build.sh                          — version owner, assembles .deb from skeleton
├── .build_counter                    — auto-incremented build number
├── index.html                        — pre-React data-theme="dark" default
├── src/
│   ├── App.tsx                       — app shell, theme detection (sole owner), step routing
│   ├── app.css                       — dual-theme CSS variables, all surfaces via vars
│   ├── main.tsx                      — React mount only
│   ├── components/
│   │   ├── StepBar.tsx               — configure / summary / write step indicator
│   │   ├── ImagesStep.tsx            — drive selection, ISO add, arch warning, type/size config
│   │   ├── SummaryStep.tsx           — drive map, ISO list, write confirmation
│   │   ├── WriteStep.tsx             — per-stage progress, log, cancel, eject reminder
│   │   └── DiskManager.tsx           — partition visualiser, format/delete/wipe/scalpel ops
│   └── lib/
│       ├── types.ts                  — TypeScript types mirroring Rust structs
│       └── api.ts                    — invoke wrappers for all Tauri commands
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json               — stamped with version by build.sh before compile
    ├── capabilities/default.json
    └── src/
        ├── main.rs                   — binary entry point
        ├── lib.rs                    — Tauri builder, plugin init, invoke handler
        ├── error.rs                  — BootOsProError type
        ├── manifest.rs               — manifest read/write, integrity hash, sanitisation
        ├── disk.rs                   — device listing, partition ops, format, mount, wipe
        ├── grub.rs                   — GRUB install, grub.cfg generation (manifest projection)
        ├── squash.rs                 — squashfs detection, extraction, live-boot probing
        ├── theme.rs                  — GTK theme detection (gsettings, returns dark: bool)
        └── commands.rs               — all Tauri commands, write pipeline, scalpel ops
```

---

## Supported distro families

### Persistent installs

| Family | Detection |
|---|---|
| Ubuntu / Mint / Debian Live / KDE Neon | `casper/filesystem.squashfs` or `live/filesystem.squashfs` |
| Fedora / RHEL / Rocky / Alma / CentOS Stream | Two-level LiveOS — `LiveOS/squashfs.img` → `rootfs.img` |
| Arch / Manjaro | `arch/x86_64/airootfs.sfs` |
| Sparky / MX / antiX | `live/filesystem.squashfs` |

### Live session boot

Ubuntu/Mint (casper), Debian/Sparky/MX (live-boot), Fedora/RHEL (grub2 label-based), Arch/Manjaro (archisolabel) — all detected by probing ISO structure and reading the volume label, not by filename.

---

## Architecture detection

Boot OS Pro checks the ELF architecture of each ISO's kernel before adding it. If an aarch64 (ARM) ISO is detected, a warning is shown — Boot OS Pro installs an x86_64 GRUB bootloader only. ARM64 dual-EFI support is planned for v1.1.

---

## Prerequisites

**Compatibility baseline:** Ubuntu 24.04+, Linux Mint 22+, Debian 13+ on x86_64. Built on Linux Mint 22.3. The binary is linked against the noble-generation glibc, so it will not start on older bases (Ubuntu 22.04, Mint 21, Debian 12) — the package declares this and will refuse to install there rather than fail silently.

**The baseline applies to the app only.** Drives written by Boot OS Pro are fully self-contained — GRUB is installed onto the drive itself and every OS boots from its own partition — so a finished drive boots on any x86_64 machine with USB boot, regardless of what that machine runs. Secure Boot must be disabled on the target machine: the drive carries unsigned GRUB with no shim chain.

In plain language, a target system needs: GRUB2 with both EFI and BIOS target payloads, the standard Linux partitioning and filesystem tools (sfdisk, mkfs for FAT/ext4/exFAT/NTFS, blkid, wipefs), squashfs-tools, parted, udev, WebKitGTK 4.1 and GTK 3 for the interface, and polkit with pkexec for privilege escalation. All of these are pulled in automatically when installing the `.deb` — including `pkexec`, which recent Ubuntu releases package separately from the polkit daemon.

Live extraction progress uses `unsquashfs -percentage` (squashfs-tools 4.6+), available across the entire baseline. The app detects older squashfs-tools at runtime and degrades to extraction without a moving percentage — never a failure.

## Runtime dependencies

These are installed automatically as `.deb` dependencies:

| Package | Provides |
|---|---|
| `libwebkit2gtk-4.1-0`, `libgtk-3-0` | application interface (WebKitGTK / GTK 3) |
| `grub2-common \| grub-common` | `grub-install` / `grub2-install` |
| `grub-efi-amd64-bin` | GRUB x86_64-efi target payload |
| `grub-pc-bin` | GRUB i386-pc (legacy BIOS) target payload |
| `dosfstools` | `mkfs.fat` |
| `exfatprogs \| exfat-utils` | `mkfs.exfat` |
| `util-linux` | `sfdisk`, `lsblk`, `blockdev`, `wipefs`, `blkid` |
| `e2fsprogs` | `mkfs.ext4` |
| `ntfs-3g` | `mkfs.ntfs` |
| `squashfs-tools` | `unsquashfs` |
| `parted` | `partprobe` |
| `udev` | `udevadm` |
| `polkitd \| policykit-1` | polkit daemon |
| `pkexec \| policykit-1` | privilege escalation launcher |

Privileged operations run through `/usr/lib/bootospro/bootospro-helper` via polkit. The helper enforces an explicit allowlist of permitted commands.

---

## Build

### Dev toolchain prerequisites

```bash
sudo apt update && sudo apt install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  libglib2.0-dev libjavascriptcoregtk-4.1-dev libsoup-3.0-dev \
  librsvg2-dev patchelf dpkg pkg-config parted exfatprogs squashfs-tools
```

Node 20+ (nvm recommended), Rust stable, Tauri CLI v2:

```bash
cargo install tauri-cli --version "^2" --locked
```

### Build and install

```bash
cd bootospro-app-V1_1
./build.sh
sudo dpkg -i dist/bootospro_1.0.X_amd64.deb
```

**Always install from `dist/`** — the Tauri bundler `.deb` lacks the polkit policy and helper binary.

**Do not use `gdebi`** — it fails on `|` alternatives in the Depends field.

### Dev build (with WebKit devtools)

```bash
cargo tauri dev
```

Right-click anywhere in the app window → Inspect. Only available in debug builds.

### Force clean recompile

```bash
cd src-tauri && cargo clean && cd ..
./build.sh
```

---

## Theming

Boot OS Pro follows the Lean Linux application theming directive. Theme detection reads `gsettings org.gnome.desktop.interface gtk-theme` via a Tauri command on mount and polls every 30 seconds for runtime switching without restart. The accent color `#4b8bd4` is fixed across both themes. All surface and text colors use CSS variables — no hardcoded hex on adaptive values.

---

## Confirmed working (on hardware)

- Full write — Mint 22.3 persistent boots, login works
- Fedora Workstation 44 live session boots
- Ubuntu 26.04 Server live added additively to existing drive, boots
- Multiple live ISOs (three ISOs on one drive, all boot from GRUB menu)
- Existing-drive add-live flow — manifest recognised, ISO copied, existing content untouched
- Disk Manager — listing, visualiser, format, delete, wipe, unmount, Clear Live ISOs
- Theme system — dark on launch, light switches correctly, 30s runtime polling
- Architecture mismatch warning — aarch64 ISOs flagged before write
- About tab — version, license, developer identity, known limitations

## Implemented — confirm on hardware

- UUID boot — persistent entries locate root by filesystem UUID with label fallback
- Streaming extraction progress — `unsquashfs -percentage` piped live to the progress bar
- Add persistent to existing drive (carve) — appends partitions to the existing table
- Manifest drift detection → Rebuild from disk reconciliation flow
- Empty Slot scalpel — reformat one slot leaving others intact
- Safe wipe — clean readable drive after wipe, filesystem selectable (fat32 default)
- Fedora/RHEL persistent extraction — two-level LiveOS squashfs, wired with banded progress

## Known outstanding (next milestone)

- Per-ISO live removal — Clear Live ISOs wipes all at once; per-ISO future work
- ARM64 dual-EFI boot — planned alongside grub-efi-arm64-bin support
- BOOTOSPRO installer partition — created and formatted, population future work
- Manual kernel/initrd override — auto-detect only, no UI fallback field

---


## Troubleshooting

**Blank window on hardened or non-standard derivatives.** Ubuntu 24.04+ restricts unprivileged user namespaces via AppArmor (`kernel.apparmor_restrict_unprivileged_userns=1`). Stock Ubuntu, Mint, and Debian run Boot OS Pro without any additional configuration. If a hardened derivative blocks the WebKit sandbox (blank window at launch), the restriction can be relaxed system-wide with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` — but on the supported baseline this should never be necessary.

## License

GPLv3 — Copyright 2026 archerprojects. See `LICENSE`.
