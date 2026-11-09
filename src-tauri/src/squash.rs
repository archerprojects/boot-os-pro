// Boot OS Pro — squash.rs — build 1.0.2 — Monday 9 November 2026

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
    ///   "archiso"  → append img_dev=/dev/disk/by-uuid/<free> img_loop=/<file> (Arch)
    ///   "none"     → family locates via its own label param
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
    /// LiveOS/squashfs.img that is really EROFS (Fedora 44+), despite the name.
    /// Mounted read-only and copied; holds either the root itself or a nested
    /// LiveOS/rootfs.img.
    ErofsLiveOS(String),
}

/// Probe a mounted ISO and return the extraction strategy.
/// Detection is purely by filesystem structure — never by filename.
pub fn probe_extraction(iso_mnt: &str) -> Result<ExtractionKind> {
    let exists = |p: &str| Path::new(&format!("{iso_mnt}/{p}")).exists();

    // Fedora/RHEL/Rocky/Alma/CentOS Stream: LiveOS layout with two-level squashfs.
    // The outer squashfs contains LiveOS/rootfs.img (an ext4 image of the root FS).
    // The filename does not tell the format: Fedora 44 ships EROFS under the
    // same name. Route on the real filesystem type.
    if exists("LiveOS/squashfs.img") {
        let img = format!("{iso_mnt}/LiveOS/squashfs.img");
        return Ok(if image_fs_type(&img) == "erofs" {
            ExtractionKind::ErofsLiveOS(img)
        } else {
            ExtractionKind::FedoraLiveOS(img)
        });
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
        ExtractionKind::ErofsLiveOS(path)    => extract_erofs_liveos(path, dest_dir, progress),
    }
}

/// Filesystem type of an image file by its content (`blkid -p`), e.g.
/// "squashfs" or "erofs". Empty if it cannot be read.
fn image_fs_type(path: &str) -> String {
    std::process::Command::new("blkid")
        .args(["-p", "-s", "TYPE", "-o", "value", path])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// EROFS LiveOS extraction (Fedora 44+): mount the image read-only and copy
/// the root out with `cp -a` (permissions, symlinks, device nodes, xattrs).
/// If the image holds a nested LiveOS/rootfs.img, mount that and copy from it
/// instead. cp has no progress, so 10 is emitted once mounted and 100 at the end.
fn extract_erofs_liveos(
    image: &str,
    dest_dir: &str,
    progress: &mut dyn FnMut(u8),
) -> Result<()> {
    use crate::disk::make_temp_dir;

    let outer_mnt = make_temp_dir()?;
    if let Err(e) = run_privileged("mount", &["-t", "erofs", "-o", "loop,ro", image, &outer_mnt]) {
        let _ = std::fs::remove_dir(&outer_mnt);
        return Err(e);
    }

    let nested = format!("{outer_mnt}/LiveOS/rootfs.img");
    let mut inner_mnt: Option<String> = None;
    let copy_root = if Path::new(&nested).exists() {
        let mnt = match make_temp_dir() {
            Ok(m) => m,
            Err(e) => {
                let _ = run_privileged("umount", &[&outer_mnt]);
                let _ = std::fs::remove_dir(&outer_mnt);
                return Err(e);
            }
        };
        if let Err(e) = run_privileged("mount", &["-o", "loop,ro", &nested, &mnt]) {
            let _ = std::fs::remove_dir(&mnt);
            let _ = run_privileged("umount", &[&outer_mnt]);
            let _ = std::fs::remove_dir(&outer_mnt);
            return Err(e);
        }
        inner_mnt = Some(mnt.clone());
        mnt
    } else {
        outer_mnt.clone()
    };
    progress(10);

    let copy_src = format!("{copy_root}/.");
    let copy_result = crate::disk::run_privileged_cancellable("cp", &["-a", &copy_src, dest_dir]);

    // Always unmount, inner first, regardless of the copy result.
    if let Some(mnt) = inner_mnt {
        let _ = run_privileged("umount", &[&mnt]);
        let _ = std::fs::remove_dir(&mnt);
    }
    let _ = run_privileged("umount", &[&outer_mnt]);
    let _ = std::fs::remove_dir(&outer_mnt);

    copy_result?;
    progress(100);
    Ok(())
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
        crate::disk::run_privileged_cancellable("unsquashfs", &[
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
/// Runs inside the privileged helper, like every other privileged step.
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
    let copy_result = crate::disk::run_privileged_cancellable("cp", &["-a", &copy_src, dest_dir]);

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
            locate: "archiso".into(),
        };
    }
    if exists("boot/vmlinuz-linux") {
        return LiveBootInfo {
            kernel: "/boot/vmlinuz-linux".into(),
            initrd: "/boot/initramfs-linux.img".into(),
            boot_params: format!("archisolabel={iso_label}"),
            locate: "archiso".into(),
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

// ── Unpacked size ──────────────────────────────────────────────────────────

/// Ratio of unpacked root filesystem to its compressed image, used when the
/// ISO does not record the real size. Mint 22.3 measures 3.2 (8.35 GB from a
/// 2.63 GB squashfs); rounded up so the estimate errs on the large side.
const UNPACK_RATIO_ESTIMATE: f64 = 3.3;

/// Unpacked size of the ISO's root filesystem in bytes, and whether it is the
/// ISO's own recorded figure (casper `filesystem.size`) or an estimate from
/// the compressed image size. None if no root image is found.
pub fn unpacked_size(iso_mnt: &str) -> Option<(u64, bool)> {
    let recorded = std::fs::read_to_string(format!("{iso_mnt}/casper/filesystem.size"))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok());
    if let Some(bytes) = recorded {
        return Some((bytes, true));
    }
    let image = match probe_extraction(iso_mnt).ok()? {
        ExtractionKind::SingleSquashfs(p)
        | ExtractionKind::FedoraLiveOS(p)
        | ExtractionKind::ErofsLiveOS(p) => p,
    };
    let compressed = std::fs::metadata(image).ok()?.len();
    Some(((compressed as f64 * UNPACK_RATIO_ESTIMATE) as u64, false))
}

// ── casper on exFAT ────────────────────────────────────────────────────────
//
// casper (Ubuntu, Mint) searches for the ISO only on filesystems named in
// is_supported_fs() in scripts/casper-helpers, and exFAT is not in that list;
// its initrd also carries no exFAT driver. A second initrd, loaded by GRUB
// after the ISO's own, fixes both: it replaces casper-helpers with the ISO's
// own copy plus exfat in the list and a line that loads the driver, and
// carries exfat.ko taken from the ISO's own root image (same kernel build).
// Proven in QEMU on Ubuntu 26.04.1 Server and Desktop and Mint 22.3.

const CASPER_FS_LIST: &str = "ext2|ext3|ext4|xfs|jfs|reiserfs|vfat|ntfs|iso9660|btrfs|udf)";
const CASPER_FS_LIST_EXFAT: &str = "ext2|ext3|ext4|xfs|jfs|reiserfs|vfat|ntfs|iso9660|btrfs|udf|exfat)";
const OVERLAY_MODULE: &str = "usr/lib/bootospro/exfat.ko";

/// Build the exFAT overlay initrd for a mounted casper ISO and write it to
/// `out`. `initrd_rel` is the ISO's initrd path, e.g. `/casper/initrd`.
pub fn build_casper_exfat_overlay(iso_mnt: &str, initrd_rel: &str, out: &str) -> Result<()> {
    let work = crate::disk::make_temp_dir()?;
    let result = build_overlay_in(&work, iso_mnt, initrd_rel, out);
    let _ = run_privileged("rm", &["-rf", &work]);
    result
}

fn build_overlay_in(work: &str, iso_mnt: &str, initrd_rel: &str, out: &str) -> Result<()> {
    let fail = |what: &str| BootOsProError::Other(format!(
        "cannot prepare this ISO to boot from exFAT: {what}"
    ));

    // 1. Unpack the ISO's initrd (handles early microcode archives and every
    //    compression casper uses).
    let unpacked = format!("{work}/initrd");
    run_privileged("unmkinitramfs", &[&format!("{iso_mnt}{initrd_rel}"), &unpacked])?;
    let root = ["main", ""].iter()
        .map(|d| if d.is_empty() { unpacked.clone() } else { format!("{unpacked}/{d}") })
        .find(|r| Path::new(&format!("{r}/scripts/casper-helpers")).exists())
        .ok_or_else(|| fail("casper-helpers not found in its initrd"))?;

    // 2. Patch casper-helpers: exfat in the filesystem list, driver loaded first.
    let helpers = std::fs::read_to_string(format!("{root}/scripts/casper-helpers"))?;
    if !helpers.contains(CASPER_FS_LIST) {
        return Err(fail("its casper version uses an unknown filesystem list"));
    }
    let (first, rest) = helpers.split_once('\n').unwrap_or((&helpers, ""));
    let patched = format!(
        "{first}\n# Boot OS Pro: load the exFAT driver so casper can find ISOs on exFAT\n\
         [ -d /sys/module/exfat ] || insmod /{OVERLAY_MODULE} 2>/dev/null\n{}",
        rest.replacen(CASPER_FS_LIST, CASPER_FS_LIST_EXFAT, 1)
    );

    // 3. The kernel version the initrd was built for.
    let kver = ["usr/lib/modules", "lib/modules"].iter()
        .filter_map(|d| std::fs::read_dir(format!("{root}/{d}")).ok())
        .flat_map(|it| it.flatten())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .ok_or_else(|| fail("no kernel modules in its initrd"))?;

    // 4. exfat.ko for that kernel, from the ISO's root image.
    let module = extract_exfat_module(work, iso_mnt, &kver)?
        .ok_or_else(|| fail(&format!("no exfat module for kernel {kver} in its root image")))?;
    let module_bytes = std::fs::read(&module)?;

    // 5. insmod is a kmod link; add one if the initrd lacks it.
    let has_insmod = ["usr/sbin/insmod", "sbin/insmod", "usr/bin/insmod", "bin/insmod"].iter()
        .any(|p| std::fs::symlink_metadata(format!("{root}/{p}")).is_ok());
    let has_kmod = Path::new(&format!("{root}/usr/bin/kmod")).exists();
    if !has_insmod && !has_kmod {
        return Err(fail("its initrd has no kmod to load the driver"));
    }

    // 6. Write the overlay as an uncompressed newc cpio archive.
    let mut cpio = CpioWriter::default();
    cpio.dir("scripts");
    cpio.file("scripts/casper-helpers", 0o644, patched.as_bytes());
    cpio.dir("usr");
    cpio.dir("usr/lib");
    cpio.dir("usr/lib/bootospro");
    cpio.file(OVERLAY_MODULE, 0o644, &module_bytes);
    if !has_insmod {
        cpio.dir("usr/sbin");
        cpio.symlink("usr/sbin/insmod", "../bin/kmod");
    }
    std::fs::write(out, cpio.finish())?;
    Ok(())
}

/// Find exfat.ko for `kver` in the ISO's casper root images and copy it,
/// decompressed, into `work`. Tries the largest image first (the full root).
fn extract_exfat_module(work: &str, iso_mnt: &str, kver: &str) -> Result<Option<String>> {
    let mut images: Vec<(u64, String)> = std::fs::read_dir(format!("{iso_mnt}/casper"))?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".squashfs"))
        .filter_map(|e| Some((e.metadata().ok()?.len(), e.path().to_string_lossy().to_string())))
        .collect();
    images.sort_by_key(|i| std::cmp::Reverse(i.0));

    let mnt = format!("{work}/root");
    std::fs::create_dir_all(&mnt)?;
    for (_, image) in images {
        if run_privileged("mount", &["-t", "squashfs", "-o", "loop,ro", &image, &mnt]).is_err() {
            continue;
        }
        let found = ["usr/lib/modules", "lib/modules"].iter()
            .flat_map(|d| ["ko.zst", "ko.xz", "ko"].map(|ext| {
                format!("{mnt}/{d}/{kver}/kernel/fs/exfat/exfat.{ext}")
            }))
            .find(|p| Path::new(p).exists());
        let copied = found.map(|src| {
            let ext = src.rsplit_once("exfat.").map(|(_, e)| e.to_string()).unwrap_or_default();
            let dest = format!("{work}/exfat.{ext}");
            std::fs::copy(&src, &dest).map(|_| dest)
        });
        let _ = run_privileged("umount", &[&mnt]);
        let Some(copied) = copied else { continue };
        let packed = copied?;
        let plain = format!("{work}/exfat.ko");
        if packed.ends_with(".zst") {
            run_privileged("zstd", &["-q", "-d", "-f", &packed, "-o", &plain])?;
        } else if packed.ends_with(".xz") {
            run_privileged("xz", &["-d", "-k", "-f", &packed])?;
        }
        return Ok(Some(plain));
    }
    Ok(None)
}

/// Minimal writer for the "newc" cpio format the kernel unpacks initrds from.
#[derive(Default)]
struct CpioWriter {
    buf: Vec<u8>,
    ino: u32,
}

impl CpioWriter {
    fn entry(&mut self, name: &str, mode: u32, nlink: u32, data: &[u8]) {
        self.ino += 1;
        let header = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            self.ino, mode, 0, 0, nlink, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
        );
        self.buf.extend_from_slice(header.as_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        self.pad();
        self.buf.extend_from_slice(data);
        self.pad();
    }

    fn pad(&mut self) {
        while !self.buf.len().is_multiple_of(4) { self.buf.push(0); }
    }

    fn dir(&mut self, name: &str) { self.entry(name, 0o040755, 2, &[]); }

    fn file(&mut self, name: &str, perm: u32, data: &[u8]) {
        self.entry(name, 0o100000 | perm, 1, data);
    }

    fn symlink(&mut self, name: &str, target: &str) {
        self.entry(name, 0o120777, 1, target.as_bytes());
    }

    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, 1, &[]);
        self.buf
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

    // Versioned kernel (Fedora): the newest vmlinuz-<version> in /boot.
    newest_boot_kernel(part_mnt).map(|v| format!("/boot/vmlinuz-{v}"))
}

/// Version of the newest regular kernel in /boot (`vmlinuz-<version>`).
/// Rescue images (`vmlinuz-0-rescue-*`) and checksum files are skipped, and
/// versions compare numerically, so the pick never depends on directory order.
fn newest_boot_kernel(part_mnt: &str) -> Option<String> {
    std::fs::read_dir(format!("{part_mnt}/boot")).ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let ver = name.strip_prefix("vmlinuz-")?.to_string();
            (!ver.contains("rescue") && !ver.ends_with(".hmac")).then_some(ver)
        })
        .max_by_key(|v| version_key(v))
}

/// Numeric parts of a version string, for ordering: "6.19.10-300" → [6, 19, 10, 300].
fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .collect()
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

    // Versioned initrd (Fedora): the one built for the kernel find_kernel picks.
    if let Some(v) = newest_boot_kernel(part_mnt) {
        let c = format!("boot/initramfs-{v}.img");
        if Path::new(&format!("{part_mnt}/{c}")).exists() {
            return Some(format!("/{c}"));
        }
    }

    // Otherwise the first initramfs-*.img that isn't a rescue image
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

#[cfg(test)]
mod tests {
    use super::{find_initrd, find_kernel, CpioWriter};

    /// Fedora /boot holds a rescue kernel next to the real one; the real,
    /// newest kernel and its own initramfs must be picked whatever the order.
    #[test]
    fn kernel_pick_skips_rescue_and_takes_newest() {
        let root = std::env::temp_dir().join(format!("bop-boot-{}", std::process::id()));
        let boot = root.join("boot");
        std::fs::create_dir_all(&boot).unwrap();
        for f in [
            "vmlinuz-0-rescue-4189730cf6c94d60bf67890447d58a3b",
            "initramfs-0-rescue-4189730cf6c94d60bf67890447d58a3b.img",
            "vmlinuz-6.9.1-100.fc44.x86_64",
            "initramfs-6.9.1-100.fc44.x86_64.img",
            "vmlinuz-6.19.10-300.fc44.x86_64",
            ".vmlinuz-6.19.10-300.fc44.x86_64.hmac",
            "initramfs-6.19.10-300.fc44.x86_64.img",
        ] {
            std::fs::write(boot.join(f), b"").unwrap();
        }
        let mnt = root.to_str().unwrap();
        let kernel = find_kernel(mnt, "fedora.iso");
        let initrd = find_initrd(mnt, "fedora.iso");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(kernel.as_deref(), Some("/boot/vmlinuz-6.19.10-300.fc44.x86_64"));
        assert_eq!(initrd.as_deref(), Some("/boot/initramfs-6.19.10-300.fc44.x86_64.img"));
    }

    /// The overlay archive must be readable by cpio itself (same newc format
    /// the kernel unpacks), with every entry, file content and link intact.
    #[test]
    fn cpio_overlay_is_valid_newc() {
        let mut c = CpioWriter::default();
        c.dir("scripts");
        c.file("scripts/casper-helpers", 0o644, b"line one\nline two\n");
        c.dir("usr");
        c.symlink("usr/insmod", "../bin/kmod");
        let archive = c.finish();
        assert_eq!(archive.len() % 4, 0);

        let dir = std::env::temp_dir().join(format!("bop-cpio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("cpio")
            .args(["-i", "-d", "--quiet", "--no-absolute-filenames"])
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&archive).unwrap();
        assert!(child.wait().unwrap().success());

        let text = std::fs::read_to_string(dir.join("scripts/casper-helpers")).unwrap();
        let link = std::fs::read_link(dir.join("usr/insmod")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(text, "line one\nline two\n");
        assert_eq!(link.to_str(), Some("../bin/kmod"));
    }
}
