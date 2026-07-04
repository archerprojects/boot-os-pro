extern crate libc;

use crate::error::{BootOsProError, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;

const HELPER: &str = "/usr/lib/bootospro/bootospro-helper";

// ── Data structures ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDevice {
    pub path: String,
    pub model: String,
    pub size_bytes: u64,
    pub size_human: String,
    pub transport: String,
    pub removable: bool,
    pub children: Vec<Partition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    pub path: String,
    pub size_bytes: u64,
    pub fstype: String,
    pub label: String,
    pub mountpoint: Option<String>,
    pub part_type: String,
}

// ── Device listing ─────────────────────────────────────────────────────────

pub fn list_usb_devices() -> Result<Vec<BlockDevice>> {
    let output = Command::new("lsblk")
        .args([
            "--json", "--bytes", "--output",
            "PATH,MODEL,SIZE,TRAN,RM,FSTYPE,LABEL,MOUNTPOINT,PARTTYPE",
        ])
        .output()?;

    if !output.status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: "lsblk".into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }

    let raw: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| BootOsProError::Other(format!("lsblk parse error: {e}")))?;

    let entries = match raw["blockdevices"].as_array() {
        Some(e) => e,
        None => return Ok(Vec::new()),
    };

    let mut devices = Vec::new();
    for d in entries {
        let transport = d["tran"].as_str().unwrap_or("").to_string();
        // Accept USB-connected drives only. Partitions in lsblk's flat output
        // have tran=null, so this also filters them out as device candidates.
        if transport != "usb" { continue; }
        let size_bytes = d["size"].as_u64().unwrap_or(0);
        if size_bytes < 1_000_000_000 { continue; } // skip hubs/boot stubs

        let dev_path = d["path"].as_str().unwrap_or("").to_string();

        // children: nested form if present, else flat siblings under this path
        let children = if d["children"].is_array() {
            parse_children(&d["children"])
        } else {
            let parts: Vec<serde_json::Value> = entries.iter()
                .filter(|e| match e["path"].as_str() {
                    Some(p) => p != dev_path && p.starts_with(&dev_path),
                    None => false,
                })
                .cloned()
                .collect();
            parse_children(&serde_json::Value::Array(parts))
        };

        devices.push(BlockDevice {
            path: dev_path,
            model: d["model"].as_str().unwrap_or("Unknown device").trim().to_string(),
            size_bytes,
            size_human: format_bytes(size_bytes),
            transport,
            removable: d["rm"].as_bool().unwrap_or(false),
            children,
        });
    }
    Ok(devices)
}

pub fn get_partition_layout(dev: &str) -> Result<BlockDevice> {
    let output = Command::new("lsblk")
        .args([
            "--json", "--bytes", "--output",
            "PATH,MODEL,SIZE,TRAN,RM,FSTYPE,LABEL,MOUNTPOINT,PARTTYPE",
            dev,
        ])
        .output()?;

    if !output.status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: "lsblk".into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }

    let raw: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| BootOsProError::Other(format!("lsblk parse error: {e}")))?;

    let entries = raw["blockdevices"]
        .as_array()
        .ok_or_else(|| BootOsProError::Other(format!("device not found: {dev}")))?;

    // lsblk may return the disk and its partitions either nested (the disk has
    // a "children" array) OR flat (the disk and each partition are siblings in
    // "blockdevices"). Which form appears depends on the lsblk version and the
    // exact column set. Handle both: find the disk entry by exact path match,
    // take partitions from its "children" if present, otherwise from siblings.
    let disk = entries.iter()
        .find(|e| e["path"].as_str() == Some(dev))
        .ok_or_else(|| BootOsProError::Other(format!("device not found: {dev}")))?;

    let size_bytes = disk["size"].as_u64().unwrap_or(0);

    let children = if disk["children"].is_array() {
        parse_children(&disk["children"])
    } else {
        let parts: Vec<serde_json::Value> = entries.iter()
            .filter(|e| match e["path"].as_str() {
                Some(p) => p != dev && p.starts_with(dev),
                None => false,
            })
            .cloned()
            .collect();
        parse_children(&serde_json::Value::Array(parts))
    };

    Ok(BlockDevice {
        path: disk["path"].as_str().unwrap_or(dev).to_string(),
        model: disk["model"].as_str().unwrap_or("Unknown device").trim().to_string(),
        size_bytes,
        size_human: format_bytes(size_bytes),
        transport: disk["tran"].as_str().unwrap_or("").to_string(),
        removable: disk["rm"].as_bool().unwrap_or(false),
        children,
    })
}

// ── Partition management ───────────────────────────────────────────────────

fn get_total_sectors(dev: &str) -> Result<u64> {
    let output = Command::new("lsblk")
        .args(["--bytes", "--nodeps", "--noheadings", "--output", "SIZE", dev])
        .output()?;

    if !output.status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: "lsblk".into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }

    let size_bytes: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| BootOsProError::Other("could not parse device size".into()))?;

    Ok(size_bytes / 512)
}

fn unmount_all(dev: &str) {
    // Pass 1: unmount by mountpoint via lsblk
    let output = Command::new("lsblk")
        .args(["--noheadings", "--list", "--output", "PATH,MOUNTPOINT", dev])
        .output();

    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let mnt = parts[1];
                if !mnt.is_empty() && mnt != "/" {
                    let _ = run_privileged("umount", &["-f", mnt]);
                }
            }
        }
    }

    // Pass 2: direct umount -f on each partition path — catches stale mounts.
    // Range covers BIOS/EFI/installer/free plus several ISO partitions.
    for n in 1..=12 {
        for part in [format!("{dev}{n}"), format!("{dev}p{n}")] {
            if std::path::Path::new(&part).exists() {
                let _ = run_privileged("umount", &["-f", &part]);
            }
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(500));
}

/// Write a new partition table for a full first write.
/// Layout: BIOS Boot(1) · EFI/ESP(2) · BOOTOSPRO installer(3) ·
///         ext4 per persistent ISO (4..) · FREESPACE FAT32 (last).
pub fn partition_drive_full(
    dev: &str,
    iso_sizes_gb: &[u64],
    free_space_gb: u64,
) -> Result<FullPartitionResult> {
    let sec_start: u64 = 2048;
    let efi_sectors: u64 = 512 * 1024 * 1024 / 512;        // 512 MiB ESP
    let bios_overhead: u64 = 2048;                          // 1 MiB BIOS Boot
    // 40 MiB cross-platform installer partition. FAT32, readable on Linux,
    // Windows, and macOS. Holds the app's installer package(s) so the drive
    // can install Boot OS Pro on any machine it is plugged into.
    let installer_sectors: u64 = 40 * 1024 * 1024 / 512;

    let total_sectors = get_total_sectors(dev)?;

    let iso_sectors: Vec<u64> = iso_sizes_gb.iter()
        .map(|&gb| gb * 1024 * 1024 * 1024 / 512)
        .collect();

    let fixed_overhead: u64 =
        sec_start + bios_overhead + efi_sectors + installer_sectors;
    let iso_total: u64 = iso_sectors.iter().sum::<u64>();

    if total_sectors < fixed_overhead + iso_total + 2048 {
        return Err(BootOsProError::Other(format!(
            "drive too small for selected ISOs — need {} sectors, have {}",
            fixed_overhead + iso_total + 2048, total_sectors
        )));
    }

    // Trim free space to whatever actually remains — absorbs GB→sector rounding
    // against real drive capacity so we never overrun the last sector.
    let available_for_free = total_sectors.saturating_sub(fixed_overhead + iso_total + 2048);
    let requested_free: u64 = if free_space_gb > 0 {
        free_space_gb * 1024 * 1024 * 1024 / 512
    } else {
        0
    };
    let free_sectors: u64 = requested_free.min(available_for_free);

    unmount_all(dev);
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Clear all existing signatures (front + GPT backup at disk end) before
    // writing the new table.
    let _ = run_privileged("wipefs", &["-a", dev]);
    run_privileged("dd", &[
        "if=/dev/zero", &format!("of={dev}"), "bs=1M", "count=1",
    ])?;
    let _ = run_privileged("udevadm", &["settle"]);

    let mut script = format!("label: gpt\ndevice: {dev}\nunit: sectors\n\n");

    // 1 — BIOS Boot Partition (GRUB i386-pc on GPT), 1 MiB, no filesystem
    script.push_str(&format!(
        "{} : start={sec_start}, size={bios_overhead}, type=21686148-6449-6E6F-744E-656564454649, name=\"BIOS Boot\"\n",
        partition_path(dev, 1)
    ));

    // 2 — EFI System Partition
    let efi_start = sec_start + bios_overhead;
    script.push_str(&format!(
        "{} : start={efi_start}, size={efi_sectors}, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=\"ESP\"\n",
        partition_path(dev, 2)
    ));

    // 3 — Installer partition (BOOTOSPRO), Microsoft basic data so it mounts
    // cleanly on Windows/macOS as a normal removable volume.
    let installer_start = efi_start + efi_sectors;
    script.push_str(&format!(
        "{} : start={installer_start}, size={installer_sectors}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"BOOTOSPRO\"\n",
        partition_path(dev, 3)
    ));

    let mut cursor = installer_start + installer_sectors;

    // 4.. — one ext4 partition per persistent ISO
    for (i, &iso_sec) in iso_sectors.iter().enumerate() {
        script.push_str(&format!(
            "{} : start={cursor}, size={iso_sec}, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"ISO{}\"\n",
            partition_path(dev, i + 4),
            i + 1
        ));
        cursor += iso_sec;
    }

    // last — free space partition (FAT32) for live-session ISOs
    if free_sectors > 0 {
        let free_idx = iso_sectors.len() + 4;
        script.push_str(&format!(
            "{} : start={cursor}, size={free_sectors}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"FREESPACE\"\n",
            partition_path(dev, free_idx)
        ));
    }

    write_sfdisk_script(dev, &script)?;

    let _ = run_privileged("partprobe", &[dev]);
    let _ = run_privileged("udevadm", &["settle"]);

    let efi_part = partition_path(dev, 2);
    let installer_part = partition_path(dev, 3);
    let iso_parts: Vec<String> =
        (0..iso_sectors.len()).map(|i| partition_path(dev, i + 4)).collect();
    let free_part = if free_sectors > 0 {
        Some(partition_path(dev, iso_sectors.len() + 4))
    } else {
        None
    };

    Ok(FullPartitionResult {
        efi_part,
        installer_part,
        iso_parts,
        free_part,
        free_space_bytes: free_sectors * 512,
    })
}

/// Carve a new ext4 persistent partition from the free space FAT32 partition.
/// Shrinks the free space partition and inserts a new ext4 partition before it.
/// Returns the new partition path and the updated free space partition path.
pub fn carve_persistent_from_free(
    dev: &str,
    new_iso_gb: u64,
) -> Result<(String, Option<String>)> {
    let layout = get_partition_layout(dev)?;

    let free_part = layout.children.iter().find(|p| {
        p.fstype.to_lowercase().contains("fat") && p.label.to_uppercase() == "FREESPACE"
    }).ok_or_else(|| BootOsProError::Other("no free space partition found on drive".into()))?;

    let free_path = free_part.path.clone();
    let free_num = partition_number(dev, &free_path)
        .ok_or_else(|| BootOsProError::Other("could not parse free space partition number".into()))?;

    // Read the TRUE start sector and size of the free space partition from the
    // on-disk partition table. sfdisk --dump is authoritative — approximating
    // from byte-sizes drifts from real sector offsets and produces invalid tables.
    let (free_start, free_sectors) = sfdisk_part_geometry(dev, free_num)?;

    let new_sectors = new_iso_gb * 1024 * 1024 * 1024 / 512;

    // Leave at least 1 MiB or the free space stub is not worth keeping.
    if new_sectors + 2048 >= free_sectors {
        return Err(BootOsProError::Other(format!(
            "not enough free space — need {} GB plus headroom, free space holds {} GB",
            new_iso_gb,
            free_sectors * 512 / 1_000_000_000
        )));
    }

    let remaining = free_sectors - new_sectors;
    let has_remaining = remaining >= 2048;

    unmount_all(dev);
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Delete the free space partition via --delete (CLI arg, not script input),
    // then add the new partitions in a single sfdisk script pass.
    // The script must include the GPT label header so sfdisk knows the table type.
    run_privileged("sfdisk", &["--delete", dev, &free_num.to_string()])?;
    let _ = run_privileged("partprobe", &[dev]);
    let _ = run_privileged("udevadm", &["settle"]);

    // CRITICAL: the new partitions are appended to the EXISTING table via
    // `sfdisk --append`. A full script pass with a `label: gpt` header would
    // create a brand-new table containing ONLY the listed partitions —
    // destroying the BIOS Boot, ESP, installer, and every persistent partition
    // entry. Append mode takes bare partition lines with no label header.
    let new_idx = free_num as usize;
    let mut add_script = format!(
        "{} : start={free_start}, size={new_sectors}, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4\n",
        partition_path(dev, new_idx)
    );
    if has_remaining {
        add_script.push_str(&format!(
            "{} : start={}, size={remaining}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"FREESPACE\"\n",
            partition_path(dev, new_idx + 1),
            free_start + new_sectors
        ));
    }

    append_sfdisk_script(dev, &add_script)?;
    let _ = run_privileged("partprobe", &[dev]);
    let _ = run_privileged("udevadm", &["settle"]);

    let new_part = partition_path(dev, new_idx);
    let new_free = if has_remaining {
        Some(partition_path(dev, new_idx + 1))
    } else {
        None
    };

    Ok((new_part, new_free))
}

/// Read a partition's (start_sector, size_sectors) from `sfdisk --dump`.
/// Authoritative source for partition geometry — unlike lsblk byte sizes,
/// it gives exact on-disk sector offsets including alignment gaps.
fn sfdisk_part_geometry(dev: &str, part_num: u32) -> Result<(u64, u64)> {
    let out = run_privileged("sfdisk", &["--dump", dev])?;
    let target = partition_path(dev, part_num as usize);

    // sfdisk --dump format: "/dev/sdbN : start= X, size= Y, type=..., ..."
    // The partition path and key=value pairs are separated by " : ".
    // Split on " : " first to isolate the key=value section, then split on ",".
    for line in out.lines() {
        let line_trim = line.trim();
        if !line_trim.starts_with(&target) { continue; }
        let kv_section = match line_trim.splitn(2, " : ").nth(1) {
            Some(s) => s,
            None => continue,
        };
        let mut start: Option<u64> = None;
        let mut size: Option<u64> = None;
        for field in kv_section.split(',') {
            let f = field.trim();
            if let Some(v) = f.strip_prefix("start=") {
                start = v.trim().parse().ok();
            } else if let Some(v) = f.strip_prefix("size=") {
                size = v.trim().parse().ok();
            }
        }
        if let (Some(s), Some(z)) = (start, size) {
            return Ok((s, z));
        }
    }
    Err(BootOsProError::Other(format!(
        "could not read geometry for {target} from sfdisk dump"
    )))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullPartitionResult {
    pub efi_part: String,
    pub installer_part: String,
    pub iso_parts: Vec<String>,
    pub free_part: Option<String>,
    /// ACTUAL size of the free space partition in bytes, after trimming the
    /// requested size to what physically fits. The manifest must record this,
    /// not the user-requested GB — the requested value can exceed reality and
    /// every downstream carve/live-add ceiling derives from the manifest.
    pub free_space_bytes: u64,
}

// ── Format ─────────────────────────────────────────────────────────────────

pub fn format_efi(efi_part: &str) -> Result<()> {
    run_privileged("mkfs.fat", &["-F", "32", "-n", "ESP", efi_part])?;
    Ok(())
}

pub fn format_iso_partition(part: &str, label: &str) -> Result<()> {
    run_privileged("mkfs.ext4", &["-F", "-L", label, part])?;
    Ok(())
}

pub fn format_free_space(part: &str) -> Result<()> {
    run_privileged("mkfs.fat", &["-F", "32", "-n", "FREESPACE", part])?;
    Ok(())
}

/// Format the 40 MiB cross-platform installer partition as FAT32.
pub fn format_installer(part: &str) -> Result<()> {
    run_privileged("mkfs.fat", &["-F", "32", "-n", "BOOTOSPRO", part])?;
    Ok(())
}

/// Format a single partition with the specified filesystem (Disk Manager).
pub fn format_partition(part: &str, fstype: &str, label: &str) -> Result<()> {
    match fstype {
        "exfat"          => run_privileged("mkfs.exfat", &["-n", label, part])?,
        "fat32" | "vfat" => run_privileged("mkfs.fat", &["-F", "32", "-n", label, part])?,
        "ext4"           => run_privileged("mkfs.ext4", &["-F", "-L", label, part])?,
        "ntfs"           => run_privileged("mkfs.ntfs", &["-f", "-L", label, part])?,
        _ => return Err(BootOsProError::Other(format!("unsupported filesystem: {fstype}"))),
    };
    Ok(())
}

// ── Mount ──────────────────────────────────────────────────────────────────

pub fn make_temp_dir() -> Result<String> {
    let out = Command::new("mktemp").arg("-d").output()?;
    if !out.status.success() {
        return Err(BootOsProError::Other("mktemp -d failed".into()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Mount an ext4 partition tuned for a one-shot bulk write (squashfs extraction).
/// Disables atime and frequent journal commits so the hundreds of thousands of
/// small-file writes don't each trigger a flush to slow USB flash. A final
/// sync() guarantees durability before unmount.
///
/// NOTE: `nobarrier` is NOT used — it was removed from ext4 in kernel 4.19,
/// and including it makes the whole mount fail on every target kernel,
/// silently defeating the fast path via the fallback below.
pub fn mount_partition_fast(part: &str, mnt: &str) -> Result<()> {
    // Try the fast option set; fall back to a plain mount so extraction still
    // proceeds (just slower) if the kernel rejects any option.
    let fast = run_privileged("mount", &[
        "-o", "noatime,data=writeback,commit=999", part, mnt,
    ]);
    if fast.is_err() {
        run_privileged("mount", &[part, mnt])?;
    }
    run_privileged("chmod", &["777", mnt])?;
    Ok(())
}

/// Mount a FAT32 partition with uid/gid options so current user can write.
pub fn mount_partition_fat(part: &str, mnt: &str) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let opts = format!("uid={uid},gid={gid},umask=000");
    run_privileged("mount", &["-o", &opts, part, mnt])?;
    Ok(())
}

// ── Disk Manager ops ───────────────────────────────────────────────────────

pub fn delete_partition(dev: &str, part_num: u32) -> Result<()> {
    unmount_all(dev);
    std::thread::sleep(std::time::Duration::from_millis(300));
    run_privileged("sfdisk", &["--delete", dev, &part_num.to_string()])?;
    let _ = run_privileged("partprobe", &[dev]);
    let _ = run_privileged("udevadm", &["settle"]);
    Ok(())
}

/// Wipe the entire device and leave it as a single, universally usable
/// partition. `fstype` is the user's choice from the Disk Manager dropdown
/// (default fat32 — readable on Linux, Windows, and macOS; a wiped stick
/// should work everywhere, which ext4 does not). The DOS partition type ID
/// matches the filesystem so other tooling reads the table honestly.
pub fn wipe_device(dev: &str, label: &str, fstype: &str) -> Result<()> {
    // Validate up front — mkfs dispatch would also catch it, but failing
    // before wiping the drive beats failing after.
    let type_id = match fstype {
        "fat32" | "vfat" => "c",  // W95 FAT32 (LBA)
        "exfat" | "ntfs" => "7",  // HPFS/NTFS/exFAT
        "ext4"           => "83", // Linux
        _ => return Err(BootOsProError::Other(format!("unsupported filesystem: {fstype}"))),
    };

    unmount_all(dev);
    std::thread::sleep(std::time::Duration::from_millis(500));

    // wipefs -a removes ALL filesystem and partition-table signatures, including
    // the GPT backup header at the END of the disk. Zeroing only the front (the
    // old approach) left the secondary GPT intact, producing a hybrid state the
    // kernel could not reconcile.
    run_privileged("wipefs", &["-a", dev])?;

    // Zero the first MiB as belt-and-braces for any stray boot signatures.
    run_privileged("dd", &[
        "if=/dev/zero",
        &format!("of={dev}"),
        "bs=1M",
        "count=1",
    ])?;

    let _ = run_privileged("udevadm", &["settle"]);

    let total_sectors = get_total_sectors(dev)?;
    let sec_start: u64 = 2048;
    let size = total_sectors - sec_start;

    let script = format!(
        "label: dos\ndevice: {dev}\nunit: sectors\n\n{dev}1 : start={sec_start}, size={size}, type={type_id}\n"
    );
    write_sfdisk_script(dev, &script)?;

    let _ = run_privileged("partprobe", &[dev]);
    let _ = run_privileged("udevadm", &["settle"]);

    let part = partition_path(dev, 1);
    format_partition(&part, fstype, label)?;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Canonical partition-path builder. NVMe (`nvme0n1`), MMC (`mmcblk0`), and
/// loop devices use a `p` separator (`nvme0n1p2`); SATA/USB (`sda`) do not.
pub fn partition_path(dev: &str, n: usize) -> String {
    if needs_p_separator(dev) {
        format!("{dev}p{n}")
    } else {
        format!("{dev}{n}")
    }
}

fn needs_p_separator(dev: &str) -> bool {
    dev.contains("nvme")
        || dev.contains("mmcblk")
        || dev.contains("loop")
        || std::path::Path::new(&format!("{dev}p1")).exists()
}

/// Extract the trailing partition number from a partition path, correctly
/// handling the `p` separator. `/dev/sdb3` → 3, `/dev/nvme0n1p2` → 2.
pub fn partition_number(dev: &str, part_path: &str) -> Option<u32> {
    let tail = part_path.strip_prefix(dev)?;
    let tail = tail.strip_prefix('p').unwrap_or(tail);
    tail.parse().ok()
}

/// Read an ISO file's volume label via blkid. Needed by Fedora/Arch live boot
/// (root=live:CDLABEL= / archisolabel=). Empty string if unreadable.
pub fn iso_volume_label(iso_path: &str) -> String {
    let out = Command::new("blkid")
        .args(["-s", "LABEL", "-o", "value", iso_path])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Read a partition's filesystem UUID via blkid. Empty string if none.
/// Used by the manifest layer as the primary drift signal.
pub fn partition_uuid(part: &str) -> String {
    let out = Command::new("blkid")
        .args(["-s", "UUID", "-o", "value", part])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// Pipe a FULL partition-table script to sfdisk. Replaces the entire table —
/// the script must carry a `label:` header. Used only for first writes and
/// whole-device operations where replacing the table is the intent.
fn write_sfdisk_script(dev: &str, script: &str) -> Result<()> {
    run_sfdisk_stdin(&["--no-reread", "--force", dev], script)
}

/// Pipe bare partition lines to `sfdisk --append`. Adds partitions to the
/// EXISTING table without touching entries not listed. The script must NOT
/// contain a `label:` header. Used by the carve path.
fn append_sfdisk_script(dev: &str, script: &str) -> Result<()> {
    run_sfdisk_stdin(&["--append", "--no-reread", "--force", dev], script)
}

fn run_sfdisk_stdin(args: &[&str], script: &str) -> Result<()> {
    let mut full_args = vec![HELPER, "sfdisk"];
    full_args.extend_from_slice(args);
    let mut child = Command::new("pkexec")
        .args(&full_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(script.as_bytes())?;
        // stdin drops here, closing the pipe so sfdisk sees EOF.
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: "sfdisk".into(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }
    Ok(())
}

fn parse_children(val: &serde_json::Value) -> Vec<Partition> {
    val.as_array().map(|parts| {
        parts.iter().map(|p| Partition {
            path: p["path"].as_str().unwrap_or("").to_string(),
            size_bytes: p["size"].as_u64().unwrap_or(0),
            fstype: p["fstype"].as_str().unwrap_or("").to_string(),
            label: p["label"].as_str().unwrap_or("").to_string(),
            mountpoint: p["mountpoint"].as_str().map(String::from),
            part_type: p["parttype"].as_str().unwrap_or("").to_string(),
        }).collect()
    }).unwrap_or_default()
}

/// Run a privileged command with its stdout streamed line-by-line to a
/// callback while it executes. Used for unsquashfs `-percentage` progress.
/// stderr is drained on a separate thread to avoid pipe-buffer deadlock and
/// is included in the error on failure.
pub fn run_privileged_streaming(
    cmd: &str,
    args: &[&str],
    mut on_line: impl FnMut(&str),
) -> Result<()> {
    use std::io::{BufRead, BufReader, Read};

    let mut full_args = vec![HELPER, cmd];
    full_args.extend_from_slice(args);

    let mut child = Command::new("pkexec")
        .args(&full_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Drain stderr concurrently — a blocked stderr pipe would stall the child.
    let stderr_handle = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            buf
        })
    });

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            on_line(&line);
        }
    }

    let status = child.wait()?;
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if !status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: cmd.to_string(),
            stderr,
        });
    }
    Ok(())
}

pub fn run_privileged(cmd: &str, args: &[&str]) -> Result<String> {
    let mut full_args = vec![HELPER, cmd];
    full_args.extend_from_slice(args);
    let output = Command::new("pkexec").args(&full_args).output()?;
    if !output.status.success() {
        return Err(BootOsProError::CommandFailed {
            cmd: cmd.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into())
}

fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    if bytes >= GB {
        format!("{:.0} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    }
}
