use crate::error::{BootOsProError, Result};
use crate::disk::run_privileged;
use std::path::Path;

/// Live-boot information detected by probing a mounted ISO's actual structure.
/// Detection is by file existence and the ISO's real volume label — never by
/// version strings in the filename — so it holds across releases.
#[derive(Debug, Clone)]
pub struct LiveBootInfo {
    pub kernel: String,
    pub initrd: String,
    /// Boot parameters appropriate to the family, including any label-based
    /// root/scan directive. The GRUB builder appends loopback file location.
    pub boot_params: String,
    /// How GRUB locates the ISO on the FAT partition for this family:
    ///   "iso-scan" → append iso-scan/filename=/<file>  (casper)
    ///   "findiso"  → append findiso=/<file>            (live-boot)
    ///   "none"     → family locates via its own label param (Fedora, Arch)
    pub locate: String,
}

/// ISO extraction strategy — determined by probing the mounted ISO structure,
/// not by filename. Filename detection is fragile across releases and forks.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractionKind {
    /// Single squashfs → unsquashfs directly into dest (Ubuntu/Mint/Debian/Arch)
    SingleSquashfs(String),
    /// Two-level: outer squashfs → rootfs.img ext4 image → copy contents (Fedora/RHEL/Rocky/Alma)
    FedoraLiveOS(String),
}

/// Probe a mounted ISO and return the extraction strategy.
/// Detection is purely by filesystem structure — never by filename.
pub fn probe_extraction(iso_mnt: &str) -> Result<ExtractionKind> {
    let exists = |p: &str| Path::new(&format!("{iso_mnt}/{p}")).exists();

    // Fedora/RHEL/Rocky/Alma/CentOS Stream: LiveOS layout with two-level squashfs.
    // The outer squashfs contains LiveOS/rootfs.img (an ext4 image of the root FS).
    if exists("LiveOS/squashfs.img") {
        return Ok(ExtractionKind::FedoraLiveOS(
            format!("{iso_mnt}/LiveOS/squashfs.img")
        ));
    }

    // Arch/Manjaro: single squashfs at arch/x86_64/airootfs.sfs
    if exists("arch/x86_64/airootfs.sfs") {
        return Ok(ExtractionKind::SingleSquashfs(
            format!("{iso_mnt}/arch/x86_64/airootfs.sfs")
        ));
    }

    // casper (Ubuntu/Mint/KDE Neon/Cosmic)
    if exists("casper/filesystem.squashfs") {
        return Ok(ExtractionKind::SingleSquashfs(
            format!("{iso_mnt}/casper/filesystem.squashfs")
        ));
    }

    // live-boot (Debian/Sparky/MX/antiX)
    if exists("live/filesystem.squashfs") {
        return Ok(ExtractionKind::SingleSquashfs(
            format!("{iso_mnt}/live/filesystem.squashfs")
        ));
    }

    Err(BootOsProError::Other(
        "could not find a recognised root filesystem in this ISO — \
         checked LiveOS/squashfs.img, arch/x86_64/airootfs.sfs, \
         casper/filesystem.squashfs, live/filesystem.squashfs".into()
    ))
}

/// Extract the ISO root filesystem into dest_dir, choosing the right strategy.
/// `progress` receives 0–100 for the extraction as a whole; the caller bands
/// it into the overall write progress.
pub fn extract_iso(
    kind: &ExtractionKind,
    dest_dir: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    match kind {
        ExtractionKind::SingleSquashfs(path) => extract_squashfs(path, dest_dir, progress),
        ExtractionKind::FedoraLiveOS(path)   => extract_fedora_liveos(path, dest_dir, progress),
    }
}

/// Standard single-level squashfs extraction via unsquashfs.
/// Progress comes from `unsquashfs -percentage`, which prints one integer per
/// line to stdout specifically for pipe consumers. IMPORTANT: -percentage was
/// added in squashfs-tools 4.6 (Mar 2023). The baseline floor — Ubuntu 22.04,
/// Mint 21, Debian 12 — ships 4.5.x, where the flag is an invalid option and
/// would fail the whole extraction. Capability is therefore detected once at
/// runtime; on 4.5.x systems extraction runs the classic way and the bar
/// holds at the stage start — graceful degradation, never a failure.
pub fn extract_squashfs(
    squashfs_path: &str,
    dest_dir: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    unsquashfs_streaming(squashfs_path, dest_dir, progress)
}

/// Detect once whether the system unsquashfs supports `-percentage` (≥ 4.6).
/// Runs unprivileged — only parses `unsquashfs -version` output.
fn unsquashfs_supports_percentage() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let out = match std::process::Command::new("unsquashfs")
            .arg("-version")
            .output()
        {
            Ok(o) => format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => return false,
        };
        // First line: "unsquashfs version 4.6.1 (2023/03/25)"
        let ver = out
            .split_whitespace()
            .skip_while(|w| *w != "version")
            .nth(1)
            .unwrap_or("");
        let mut nums = ver
            .split('.')
            .map(|p| p.trim_matches(|c: char| !c.is_ascii_digit()))
            .filter_map(|p| p.parse::<u32>().ok());
        let major = nums.next().unwrap_or(0);
        let minor = nums.next().unwrap_or(0);
        (major, minor) >= (4, 6)
    })
}

/// Run unsquashfs, streaming percentage lines to the callback when the
/// installed version supports it; otherwise run the classic silent path.
fn unsquashfs_streaming(
    src: &str,
    dest_dir: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get()).unwrap_or(2).to_string();

    if unsquashfs_supports_percentage() {
        crate::disk::run_privileged_streaming(
            "unsquashfs",
            &["-f", "-percentage", "-processors", &nproc, "-d", dest_dir, src],
            |line| {
                if let Ok(p) = line.trim().parse::<u8>() {
                    progress(p.min(100));
                }
            },
        )
    } else {
        // squashfs-tools 4.5.x — no pipe-friendly progress available.
        run_privileged("unsquashfs", &[
            "-f", "-no-progress", "-processors", &nproc, "-d", dest_dir, src,
        ])?;
        progress(100);
        Ok(())
    }
}

/// Fedora/RHEL two-level extraction:
///   1. unsquashfs the outer LiveOS/squashfs.img to a temp staging dir
///   2. Mount the inner LiveOS/rootfs.img as a loop device read-only
///   3. cp -a its contents into dest_dir
///   4. Unmount and remove the staging dir
///
/// All privileged ops use the existing helper allowlist (mount, umount, cp, rm).
/// Progress banding: the outer unsquashfs maps to 0–70 of this extraction;
/// the rootfs.img copy has no per-file progress (cp -a), so 70 is emitted
/// before it starts and 100 when it completes.
fn extract_fedora_liveos(
    outer_squashfs: &str,
    dest_dir: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    use crate::disk::make_temp_dir;

    // Step 1: extract outer squashfs to staging dir (0–70 of the band)
    let stage_dir = make_temp_dir()?;

    if let Err(e) = unsquashfs_streaming(outer_squashfs, &stage_dir, &mut |p| {
        progress((p as u16 * 70 / 100) as u8);
    }) {
        let _ = run_privileged("rm", &["-rf", &stage_dir]);
        return Err(e);
    }
    progress(70);

    // Step 2: rootfs.img lives at stage_dir/LiveOS/rootfs.img
    let rootfs = format!("{stage_dir}/LiveOS/rootfs.img");
    if !Path::new(&rootfs).exists() {
        let _ = run_privileged("rm", &["-rf", &stage_dir]);
        return Err(BootOsProError::Other(format!(
            "Fedora LiveOS: rootfs.img not found at {rootfs} after unsquashfs"
        )));
    }

    // Step 3: mount rootfs.img as loop read-only
    let loop_mnt = match make_temp_dir() {
        Ok(m) => m,
        Err(e) => { let _ = run_privileged("rm", &["-rf", &stage_dir]); return Err(e); }
    };

    if let Err(e) = run_privileged("mount", &["-o", "loop,ro", &rootfs, &loop_mnt]) {
        let _ = run_privileged("rm", &["-rf", &stage_dir]);
        let _ = std::fs::remove_dir(&loop_mnt);
        return Err(e);
    }

    // Step 4: copy entire root filesystem — cp -a preserves permissions,
    // symlinks, and device nodes, all of which are required for a bootable root.
    let copy_src = format!("{loop_mnt}/.");
    let copy_result = run_privileged("cp", &["-a", &copy_src, dest_dir]);

    // Always clean up regardless of copy result
    let _ = run_privileged("umount", &[&loop_mnt]);
    let _ = std::fs::remove_dir(&loop_mnt);
    let _ = run_privileged("rm", &["-rf", &stage_dir]);

    copy_result?;
    progress(100);
    Ok(())
}

/// Detect live-boot info from a mounted ISO by probing its structure.
/// `iso_label` is the ISO's volume label (from blkid), needed by Fedora/Arch.
pub fn detect_live_boot(iso_mnt: &str, iso_label: &str) -> LiveBootInfo {
    let exists = |p: &str| Path::new(&format!("{iso_mnt}/{p}")).exists();

    // ── casper (Ubuntu / Mint / KDE Neon / Cosmic) ──────────────────────────
    if exists("casper/vmlinuz") {
        let initrd = ["casper/initrd.lz", "casper/initrd.img", "casper/initrd"]
            .iter().find(|p| exists(p)).unwrap_or(&"casper/initrd").to_string();
        return LiveBootInfo {
            kernel: "/casper/vmlinuz".into(),
            initrd: format!("/{initrd}"),
            boot_params: "boot=casper quiet splash".into(),
            locate: "iso-scan".into(),
        };
    }

    // ── live-boot (Debian / Sparky / MX / antiX) ────────────────────────────
    if exists("live/vmlinuz") || exists("live/vmlinuz1") {
        let kernel = if exists("live/vmlinuz") { "live/vmlinuz" } else { "live/vmlinuz1" };
        let initrd = ["live/initrd.img", "live/initrd1.img", "live/initrd.lz"]
            .iter().find(|p| exists(p)).unwrap_or(&"live/initrd.img").to_string();
        return LiveBootInfo {
            kernel: format!("/{kernel}"),
            initrd: format!("/{initrd}"),
            boot_params: "boot=live components quiet splash".into(),
            locate: "findiso".into(),
        };
    }

    // ── Fedora / RHEL family ─────────────────────────────────────────────────
    if exists("boot/x86_64/loader/linux") {
        return LiveBootInfo {
            kernel: "/boot/x86_64/loader/linux".into(),
            initrd: "/boot/x86_64/loader/initrd".into(),
            boot_params: format!("root=live:CDLABEL={iso_label} rd.live.image quiet rhgb"),
            locate: "iso-scan".into(),
        };
    }
    if exists("isolinux/vmlinuz") && exists("LiveOS/squashfs.img") {
        return LiveBootInfo {
            kernel: "/isolinux/vmlinuz".into(),
            initrd: "/isolinux/initrd.img".into(),
            boot_params: format!("root=live:CDLABEL={iso_label} rd.live.image quiet rhgb"),
            locate: "iso-scan".into(),
        };
    }

    // ── Arch / Manjaro ───────────────────────────────────────────────────────
    if exists("arch/boot/x86_64/vmlinuz-linux") {
        return LiveBootInfo {
            kernel: "/arch/boot/x86_64/vmlinuz-linux".into(),
            initrd: "/arch/boot/x86_64/initramfs-linux.img".into(),
            boot_params: format!("archisolabel={iso_label}"),
            locate: "none".into(),
        };
    }
    if exists("boot/vmlinuz-linux") {
        return LiveBootInfo {
            kernel: "/boot/vmlinuz-linux".into(),
            initrd: "/boot/initramfs-linux.img".into(),
            boot_params: format!("archisolabel={iso_label}"),
            locate: "none".into(),
        };
    }

    // ── Fallback ─────────────────────────────────────────────────────────────
    let kernel = ["boot/vmlinuz", "vmlinuz"].iter().find(|p| exists(p))
        .unwrap_or(&"casper/vmlinuz").to_string();
    let initrd = ["boot/initrd.img", "initrd.img", "boot/initrd"].iter().find(|p| exists(p))
        .unwrap_or(&"casper/initrd.lz").to_string();
    LiveBootInfo {
        kernel: format!("/{kernel}"),
        initrd: format!("/{initrd}"),
        boot_params: "quiet splash".into(),
        locate: "iso-scan".into(),
    }
}

/// Mount an ISO file read-only.
pub fn mount_iso(iso_path: &str, mnt: &str) -> Result<()> {
    run_privileged("mount", &["-o", "loop,ro", iso_path, mnt])?;
    Ok(())
}

/// Find the kernel inside an extracted partition root.
/// Probes by actual file existence. For Fedora, scans /boot for vmlinuz-* files.
pub fn find_kernel(part_mnt: &str, iso_name: &str) -> Option<String> {
    let n = iso_name.to_lowercase();

    if n.contains("arch") || n.contains("manjaro") {
        for c in &["boot/vmlinuz-linux", "arch/boot/x86_64/vmlinuz-linux"] {
            if Path::new(&format!("{part_mnt}/{c}")).exists() {
                return Some(format!("/{c}"));
            }
        }
    }

    // Fixed candidates cover casper, live-boot, and plain /boot/vmlinuz
    for c in &["casper/vmlinuz", "live/vmlinuz", "boot/vmlinuz", "vmlinuz"] {
        if Path::new(&format!("{part_mnt}/{c}")).exists() {
            return Some(format!("/{c}"));
        }
    }

    // Fedora versioned kernel: first vmlinuz-* in /boot that isn't a checksum
    if let Ok(entries) = std::fs::read_dir(format!("{part_mnt}/boot")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("vmlinuz-") && !s.ends_with(".hmac") {
                return Some(format!("/boot/{s}"));
            }
        }
    }

    None
}

/// Find the initrd inside an extracted partition root.
/// Probes by actual file existence. For Fedora, scans /boot for initramfs-* files.
pub fn find_initrd(part_mnt: &str, iso_name: &str) -> Option<String> {
    let n = iso_name.to_lowercase();

    if n.contains("arch") || n.contains("manjaro") {
        for c in &["boot/initramfs-linux.img", "arch/boot/x86_64/initramfs-linux.img"] {
            if Path::new(&format!("{part_mnt}/{c}")).exists() {
                return Some(format!("/{c}"));
            }
        }
    }

    for c in &[
        "casper/initrd.lz", "casper/initrd", "casper/initrd.img",
        "live/initrd.lz", "live/initrd", "live/initrd.img",
        "boot/initrd.img", "initrd.img",
    ] {
        if Path::new(&format!("{part_mnt}/{c}")).exists() {
            return Some(format!("/{c}"));
        }
    }

    // Fedora versioned initrd: first initramfs-*.img that isn't a rescue image
    if let Ok(entries) = std::fs::read_dir(format!("{part_mnt}/boot")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("initramfs-") && s.ends_with(".img") && !s.contains("rescue") {
                return Some(format!("/boot/{s}"));
            }
        }
    }

    None
}

/// Read the ELF architecture from a kernel inside a mounted ISO.
/// Returns "x86_64", "aarch64", or "unknown".
/// Used to warn the user before writing an incompatible ISO.
pub fn detect_iso_arch(iso_mnt: &str) -> &'static str {
    let candidates = [
        "casper/vmlinuz",
        "live/vmlinuz",
        "boot/x86_64/loader/linux",
        "isolinux/vmlinuz",
        "arch/boot/x86_64/vmlinuz-linux",
        "boot/vmlinuz",
        "vmlinuz",
    ];
    for candidate in &candidates {
        let path = format!("{iso_mnt}/{candidate}");
        if Path::new(&path).exists() {
            return read_elf_arch(&path);
        }
    }
    "unknown"
}

/// Read the ELF machine type from the first 20 bytes of a file.
/// ELF magic: bytes 0-3. Machine type: bytes 18-19 (little-endian u16).
/// 0x003E = x86_64, 0x00B7 = AArch64.
fn read_elf_arch(path: &str) -> &'static str {
    use std::io::Read;
    let mut buf = [0u8; 20];
    let Ok(mut f) = std::fs::File::open(path) else { return "unknown"; };
    let Ok(n) = f.read(&mut buf) else { return "unknown"; };
    if n < 20 || &buf[0..4] != b"\x7fELF" { return "unknown"; }
    match u16::from_le_bytes([buf[18], buf[19]]) {
        0x003E => "x86_64",
        0x00B7 => "aarch64",
        _      => "unknown",
    }
}
