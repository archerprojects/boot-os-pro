// Boot OS Pro — jobs.rs — build 1.0.2 — Monday 9 November 2026
//
// Every privileged operation, as a job. The privileged helper runs one job
// per pkexec launch (one password prompt per operation) and streams progress
// back to the app; the app never runs a privileged step itself.

use crate::disk;
use crate::error::{BootOsProError, Result};
use crate::grub::{self, PersistentEntry, LiveEntry};
use crate::manifest::{
    self, Manifest, PersistentRecord, LiveRecord, FreeSpaceRecord,
    InstallerRecord, SlotState,
};
use crate::squash;
use std::path::Path;
use serde::{Deserialize, Serialize};

// ── Cancellation flag ──────────────────────────────────────────────────────

static CANCELLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_cancelled(v: bool) {
    CANCELLED.store(v, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(std::sync::atomic::Ordering::SeqCst)
}

// ── Progress ───────────────────────────────────────────────────────────────

/// Where a job reports progress. The helper writes each event to stdout as a
/// JSON line; the app relays them to the frontend as `write_progress`.
pub trait Progress {
    fn emit(&self, stage: &str, pct: u8, msg: &str);
}

// ── Input specs from the frontend ──────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct IsoWriteSpec {
    pub name: String,
    pub path: String,
    pub label: String,
    pub size_gb: u64,
    pub boot_params: String,
    pub username: String,
    pub password: String,
}

/// Live ISO input from the frontend. Boot params are NOT accepted here —
/// they are detected from the ISO's actual structure at write time.
#[derive(Clone, Serialize, Deserialize)]
pub struct LiveIsoSpec {
    pub name: String,
    pub path: String,
}

/// An empty ext4 slot reserved on a new drive for a persistent OS added later.
#[derive(Clone, Serialize, Deserialize)]
pub struct ReservedSlotSpec {
    pub label: String,
    pub size_gb: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FullWriteArgs {
    pub device: String,
    pub drive_label: String,
    pub isos: Vec<IsoWriteSpec>,
    #[serde(default)]
    pub reserved: Vec<ReservedSlotSpec>,
    pub free_space_gb: u64,
    pub live_isos: Vec<LiveIsoSpec>,
}

// ── Full first write pipeline ──────────────────────────────────────────────

pub fn run_full_write(args: FullWriteArgs, app_version: &str, p: &dyn Progress) -> Result<()> {
    set_cancelled(false);

    // Persistent ISOs first, then reserved slots: both are ext4 partitions.
    let iso_sizes: Vec<u64> = args.isos.iter().map(|i| i.size_gb)
        .chain(args.reserved.iter().map(|r| r.size_gb))
        .collect();

    // 1. Partition (includes the BOOTOSPRO installer partition)
    p.emit("partition", 0, "Partitioning drive…");
    let parts = disk::partition_drive_full(&args.device, &iso_sizes, args.free_space_gb)?;
    let slot_parts = &parts.iso_parts[args.isos.len()..];
    p.emit("partition", 100, "Drive partitioned.");

    // 2. Format EFI, installer, free space, and each ISO partition
    p.emit("format", 0, "Formatting EFI partition…");
    disk::format_efi(&parts.efi_part)?;

    p.emit("format", 15, "Formatting installer partition…");
    disk::format_installer(&parts.installer_part)?;

    if let Some(ref fp) = parts.free_part {
        p.emit("format", 30, "Formatting free space partition…");
        disk::format_free_space(fp)?;
    }

    p.emit("format", 50, "Formatting OS partitions…");
    for (i, iso) in args.isos.iter().enumerate() {
        if is_cancelled() { return Err(BootOsProError::Cancelled); }
        disk::format_iso_partition(&parts.iso_parts[i], &iso.label)?;
    }
    for (slot, part) in args.reserved.iter().zip(slot_parts) {
        if is_cancelled() { return Err(BootOsProError::Cancelled); }
        disk::format_iso_partition(part, &slot.label)?;
    }
    p.emit("format", 100, "Partitions formatted.");

    // 3. Mount EFI and install GRUB
    p.emit("grub", 0, "Mounting EFI partition…");
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&parts.efi_part, &efi_mnt)?;

    p.emit("grub", 20, "Installing GRUB (EFI + BIOS)…");
    if let Err(e) = grub::install_grub(&efi_mnt, &args.device) {
        cleanup_mount(&efi_mnt);
        return Err(e);
    }
    p.emit("grub", 100, "GRUB installed.");

    // Steps 4–8 run with the EFI partition mounted. Any error (or cancel) must
    // unmount it — run the body as a closure and clean up on the way out.
    let body = || -> Result<()> {
        // 4. Extract each persistent ISO into its partition, build records.
        //    Each ISO owns an equal band of the extract stage so progress never
        //    moves backwards across ISOs: ISO i spans [i, i+1) * 100/total.
        let total = args.isos.len().max(1);
        let mut persistent_records: Vec<PersistentRecord> = Vec::new();

        for (i, iso) in args.isos.iter().enumerate() {
            if is_cancelled() { return Err(BootOsProError::Cancelled); }

            let base_pct = (i * 100 / total) as u8;
            let span_pct = (100 / total) as u8;

            p.emit("extract", base_pct,
                &format!("Extracting {} ({}/{})…", iso.name, i + 1, total));

            let (kernel, initrd, fs_uuid) = extract_persistent(p, &iso.path, &parts.iso_parts[i], iso, base_pct, span_pct,
            )?;

            persistent_records.push(PersistentRecord {
                label: manifest::sanitize_label(&iso.label),
                os_name: Some(iso.name.clone()),
                kernel,
                initrd,
                boot_params: manifest::sanitize_boot_params(&iso.boot_params),
                size_bytes: iso.size_gb * 1024 * 1024 * 1024,
                fs_uuid,
                state: SlotState::Filled,
            });

            p.emit("extract", base_pct.saturating_add(span_pct),
                &format!("{} extracted.", iso.name));
        }

        // Reserved slots: empty ext4 partitions recorded for a later install.
        for (slot, part) in args.reserved.iter().zip(slot_parts) {
            persistent_records.push(PersistentRecord {
                label: manifest::sanitize_label(&slot.label),
                os_name: None,
                kernel: String::new(),
                initrd: String::new(),
                boot_params: String::new(),
                size_bytes: slot.size_gb * 1024 * 1024 * 1024,
                fs_uuid: disk::partition_uuid(part),
                state: SlotState::Empty,
            });
        }

        // 5. Copy live ISOs onto the free space partition, one at a time. A
        //    failed copy stops the live copies but not the write: the boot
        //    menu and manifest are still written for everything that is on
        //    the drive, and the error is reported after that.
        let mut live_records: Vec<LiveRecord> = Vec::new();
        let mut live_error: Option<BootOsProError> = None;
        let free_uuid = if let Some(ref fp) = parts.free_part {
            for live in &args.live_isos {
                match copy_live_isos(p, fp, std::slice::from_ref(live)) {
                    Ok(recs) => live_records.extend(recs),
                    Err(BootOsProError::Cancelled) => return Err(BootOsProError::Cancelled),
                    Err(e) => { live_error = Some(e); break; }
                }
            }
            disk::partition_uuid(fp)
        } else {
            String::new()
        };

        // 6. Write GRUB config from the records
        p.emit("config", 0, "Writing GRUB configuration…");
        let persistent_entries = persistent_entries_from_records(&persistent_records);
        let live_entries = live_entries_from_records(&live_records, &free_uuid);
        grub::write_grub_config(&efi_mnt, &persistent_entries, &live_entries)?;

        // 7. Build and write the manifest (master record on EFI).
        //    free_space records the ACTUAL trimmed partition size from
        //    partitioning, never the user-requested GB — all carve and
        //    live-add ceilings downstream derive from this value.
        let mut m = Manifest::new(&args.drive_label, app_version);
        m.persistent = persistent_records;
        m.live = live_records;
        m.free_space = FreeSpaceRecord {
            label: if parts.free_part.is_some() { "FREESPACE".into() } else { String::new() },
            size_bytes: parts.free_space_bytes,
            fs_uuid: free_uuid,
        };
        m.installer = InstallerRecord { present: true, packages: Vec::new() };
        write_manifest(&efi_mnt, &m)?;
        p.emit("config", 100, "Configuration written.");

        // 8. Sync
        p.emit("sync", 0, "Syncing to disk…");
        sync_with_progress(p, "sync", 0)?;

        match live_error {
            Some(e) => Err(BootOsProError::Other(format!(
                "{e}\n\nThe drive still boots: its boot menu lists the persistent OSes and \
                 the live ISOs copied before this error."
            ))),
            None => Ok(()),
        }
    };

    let result = body();
    cleanup_mount(&efi_mnt);
    result?;

    p.emit("sync", 100, "Done! USB drive is ready.");
    Ok(())
}

// ── Add persistent ISO to existing drive ──────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct AddPersistentArgs {
    pub device: String,
    pub iso: IsoWriteSpec,
}

pub fn add_persistent_iso(args: AddPersistentArgs, p: &dyn Progress) -> Result<()> {
    set_cancelled(false);

    p.emit("partition", 0, "Carving partition from free space…");
    let carve = disk::carve_persistent_from_free(&args.device, args.iso.size_gb)?;
    let new_part = carve.new_part.clone();

    // The carve re-creates the free space partition at a new start sector, so
    // its old filesystem and every live ISO in it are gone. Format it with the
    // filesystem it had, and record that in the manifest NOW — before the long
    // extraction — so a later failure never leaves GRUB listing missing ISOs.
    let free_uuid = match carve.free_part {
        Some(ref fp) => {
            p.emit("partition", 50, "Re-formatting free space…");
            disk::format_free_space_as(fp, &carve.free_fstype)?;
            disk::partition_uuid(fp)
        }
        None => String::new(),
    };
    {
        let efi_part = find_efi(&args.device)?;
        let efi_mnt = disk::make_temp_dir()?;
        disk::mount_partition_fat(&efi_part, &efi_mnt)?;
        let result = (|| -> Result<()> {
            let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
            m.live.clear();
            m.free_space = FreeSpaceRecord {
                label: if carve.free_part.is_some() { "FREESPACE".into() } else { String::new() },
                size_bytes: carve.free_bytes,
                fs_uuid: free_uuid,
            };
            rewrite_grub_and_manifest(&efi_mnt, &mut m)
        })();
        cleanup_mount(&efi_mnt);
        result?;
    }
    p.emit("partition", 100, "Partition created.");

    p.emit("format", 0, &format!("Formatting partition for {}…", args.iso.name));
    disk::format_iso_partition(&new_part, &args.iso.label)?;
    p.emit("format", 100, "Partition formatted.");

    p.emit("extract", 0, &format!("Extracting {}…", args.iso.name));
    let (kernel, initrd, fs_uuid) =
        extract_persistent(p, &args.iso.path, &new_part, &args.iso, 0, 100)?;
    p.emit("extract", 100, &format!("{} extracted.", args.iso.name));

    // Mount EFI, load manifest, append the new record, rewrite GRUB + manifest
    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        m.persistent.retain(|p| p.label != manifest::sanitize_label(&args.iso.label));
        m.persistent.push(PersistentRecord {
            label: manifest::sanitize_label(&args.iso.label),
            os_name: Some(args.iso.name.clone()),
            kernel,
            initrd,
            boot_params: manifest::sanitize_boot_params(&args.iso.boot_params),
            size_bytes: args.iso.size_gb * 1024 * 1024 * 1024,
            fs_uuid,
            state: SlotState::Filled,
        });

        p.emit("config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        sync_with_progress(p, "config", 50)?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    p.emit("config", 100, "Drive updated.");

    Ok(())
}

// ── Install a persistent ISO into an empty reserved slot ───────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct FillSlotArgs {
    pub device: String,
    /// Label of the empty slot, as recorded in the manifest.
    pub slot_label: String,
    pub iso: IsoWriteSpec,
}

/// Install a persistent OS into an existing empty slot. Unlike add-persistent
/// nothing is carved, so the free space partition and its live ISOs are kept.
pub fn fill_slot(args: FillSlotArgs, p: &dyn Progress) -> Result<()> {
    set_cancelled(false);
    let slot_label = manifest::sanitize_label(&args.slot_label);

    // The slot must be recorded empty in the manifest and exist on the drive.
    let efi_part = find_efi(&args.device)?;
    {
        let efi_mnt = disk::make_temp_dir()?;
        disk::mount_partition_fat(&efi_part, &efi_mnt)?;
        let m = load_or_rebuild_manifest(&efi_mnt, &args.device);
        cleanup_mount(&efi_mnt);
        let empty = m?.persistent.iter()
            .any(|r| r.label == slot_label && r.state == SlotState::Empty);
        if !empty {
            return Err(BootOsProError::Other(format!(
                "{slot_label} is not an empty slot on this drive"
            )));
        }
    }
    let layout = disk::get_partition_layout(&args.device)?;
    let slot = layout.children.iter()
        .find(|c| c.fstype == "ext4" && c.label.to_uppercase() == slot_label)
        .ok_or_else(|| BootOsProError::Other(format!("partition {slot_label} not found")))?;
    let part = slot.path.clone();
    let size_bytes = slot.size_bytes;

    let _ = disk::run_privileged("umount", &[&part]);
    p.emit("format", 0, &format!("Formatting {slot_label} for {}…", args.iso.name));
    disk::format_iso_partition(&part, &args.iso.label)?;
    p.emit("format", 100, "Partition formatted.");

    p.emit("extract", 0, &format!("Extracting {}…", args.iso.name));
    let (kernel, initrd, fs_uuid) =
        extract_persistent(p, &args.iso.path, &part, &args.iso, 0, 100)?;
    p.emit("extract", 100, &format!("{} extracted.", args.iso.name));

    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;
    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        let new_label = manifest::sanitize_label(&args.iso.label);
        m.persistent.retain(|r| r.label != slot_label && r.label != new_label);
        m.persistent.push(PersistentRecord {
            label: new_label,
            os_name: Some(args.iso.name.clone()),
            kernel,
            initrd,
            boot_params: manifest::sanitize_boot_params(&args.iso.boot_params),
            size_bytes,
            fs_uuid,
            state: SlotState::Filled,
        });
        p.emit("config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        sync_with_progress(p, "config", 50)
    })();
    cleanup_mount(&efi_mnt);
    result?;
    p.emit("config", 100, "Drive updated.");
    Ok(())
}

// ── Add live ISO to existing drive (additive — no reformat) ────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct AddLiveArgs {
    pub device: String,
    pub iso: LiveIsoSpec,
}

pub fn add_live_iso(args: AddLiveArgs, p: &dyn Progress) -> Result<()> {
    set_cancelled(false);

    let free_part = find_free_space(&args.device)?;

    // Additive: copy the new ISO alongside any existing ones, do NOT reformat.
    let recs = copy_live_isos(p, &free_part, std::slice::from_ref(&args.iso))?;

    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        for r in recs {
            m.live.retain(|l| l.filename != r.filename);
            m.live.push(r);
        }

        p.emit("config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        sync_with_progress(p, "config", 50)?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    p.emit("config", 100, "Live ISO added.");

    Ok(())
}

// ── Scalpel: format one persistent partition in place ──────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct FormatPersistentArgs {
    pub device: String,
    pub partition: String,
    pub label: String,
}

/// Format a single persistent partition, leaving all others intact. The slot
/// becomes "Persistent Empty" in the manifest and its GRUB entry is dropped.
pub fn format_persistent_slot(args: FormatPersistentArgs, p: &dyn Progress) -> Result<()> {
    // Unmount first — mkfs fails on a mounted partition (desktop automounts it).
    let _ = disk::run_privileged("umount", &[&args.partition]);
    p.emit("format", 0, &format!("Formatting {}…", args.partition));
    disk::format_iso_partition(&args.partition, &args.label)?;
    let fs_uuid = disk::partition_uuid(&args.partition);
    p.emit("format", 60, "Partition formatted.");

    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        let label = manifest::sanitize_label(&args.label);
        // Mark the slot empty: keep the partition in the manifest as an empty
        // persistent slot, drop its boot info so no GRUB entry is generated.
        if let Some(rec) = m.persistent.iter_mut().find(|p| p.label == label) {
            rec.os_name = None;
            rec.kernel = String::new();
            rec.initrd = String::new();
            rec.boot_params = String::new();
            rec.fs_uuid = fs_uuid;
            rec.state = SlotState::Empty;
        } else {
            m.persistent.push(PersistentRecord {
                label: label.clone(),
                os_name: None,
                kernel: String::new(),
                initrd: String::new(),
                boot_params: String::new(),
                size_bytes: 0,
                fs_uuid,
                state: SlotState::Empty,
            });
        }

        p.emit("config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        sync_with_progress(p, "config", 50)?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    p.emit("config", 100, "Slot is now empty and ready for a new OS.");

    Ok(())
}

// ── Format the free space partition (wipes ALL live ISOs at once) ──────────

#[derive(Clone, Serialize, Deserialize)]
pub struct FormatFreeArgs {
    pub device: String,
}

pub fn format_free_space_slot(args: FormatFreeArgs, p: &dyn Progress) -> Result<()> {
    let free_part = find_free_space(&args.device)?;

    // Unmount first — mkfs.fat fails on a mounted partition. The desktop
    // automounts removable partitions, so clear-live must handle that itself.
    let _ = disk::run_privileged("umount", &[&free_part]);

    p.emit("format", 0, "Formatting free space — all live ISOs will be removed…");
    disk::format_free_space(&free_part)?;
    let fs_uuid = disk::partition_uuid(&free_part);
    p.emit("format", 60, "Free space formatted.");

    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        m.live.clear();
        m.free_space.fs_uuid = fs_uuid;

        p.emit("config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        sync_with_progress(p, "config", 50)?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    p.emit("config", 100, "All live ISOs removed.");

    Ok(())
}

// ── Read the drive manifest (for the drive map / reconciliation) ───────────

#[derive(Serialize, Deserialize)]
pub struct DriveReport {
    pub manifest: Option<Manifest>,
    /// True when the manifest does not match the physical drive (drift) and the
    /// frontend should offer to reconcile.
    pub drift: bool,
}

/// Read the manifest from a drive's EFI partition and check it against reality.
/// Drift = a recorded partition's filesystem UUID no longer matches (it was
/// reformatted outside the app, e.g. with GParted).
pub fn read_drive_manifest(device: String) -> Result<DriveReport> {
    let efi_part = match find_efi(&device) {
        Ok(p) => p,
        // No EFI partition → not a Boot OS Pro drive; no manifest.
        Err(_) => return Ok(DriveReport { manifest: None, drift: false }),
    };

    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let read = manifest::read_from_efi(&efi_mnt).map_err(BootOsProError::Other);
    let layout = disk::get_partition_layout(&device);
    cleanup_mount(&efi_mnt);

    let m = match read {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(DriveReport { manifest: None, drift: false }),
        Err(_) => return Ok(DriveReport { manifest: None, drift: false }),
    };

    // Drift check: every filled persistent record's UUID must match a live part.
    let drift = match layout {
        Ok(dev) => {
            let live_uuids: Vec<String> = dev.children.iter()
                .map(|p| disk::partition_uuid(&p.path))
                .collect();
            m.persistent.iter()
                .filter(|p| p.state == SlotState::Filled && !p.fs_uuid.is_empty())
                .any(|p| !live_uuids.contains(&p.fs_uuid))
        }
        Err(_) => false,
    };

    Ok(DriveReport { manifest: Some(m), drift })
}

/// Rebuild the manifest from the physical drive and write it back. Used when the
/// user accepts reconciliation after drift is detected, or when a drive made by
/// an older build has no manifest.
pub fn reconcile_manifest(device: String, drive_label: String, app_version: &str) -> Result<()> {
    let efi_part = find_efi(&device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = rebuild_manifest_from_drive(&device, &drive_label, app_version)?;
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result
}

// ── Cancellation ───────────────────────────────────────────────────────────

// ── Disk Manager ───────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct FormatPartitionArgs {
    pub partition: String,
    pub fstype: String,
    pub label: String,
}

pub fn format_partition(args: FormatPartitionArgs, p: &dyn Progress) -> Result<()> {
    // Unmount first — mkfs refuses a mounted partition, and the desktop
    // automounts USB partitions (often after the layout was last read).
    let _ = disk::run_privileged("umount", &[&args.partition]);
    p.emit("format", 0, &format!("Formatting {} as {}…", args.partition, args.fstype));
    disk::format_partition(&args.partition, &args.fstype, &args.label)?;
    p.emit("format", 100, "Done.");
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeletePartitionArgs {
    pub device: String,
    pub partition: String,
}

pub fn delete_partition(args: DeletePartitionArgs, p: &dyn Progress) -> Result<()> {
    let part_num = disk::partition_number(&args.device, &args.partition)
        .ok_or_else(|| BootOsProError::Other(
            format!("could not parse partition number from {}", args.partition)))?;
    p.emit("partition", 0, &format!("Deleting partition {}…", part_num));
    disk::delete_partition(&args.device, part_num)?;
    p.emit("partition", 100, "Partition deleted.");
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WipeDeviceArgs {
    pub device: String,
    pub label: String,
    pub fstype: String,
}

pub fn wipe_device(args: WipeDeviceArgs, p: &dyn Progress) -> Result<()> {
    p.emit("partition", 0, "Wiping device…");
    disk::wipe_device(&args.device, &args.label, &args.fstype)?;
    p.emit("partition", 100,
        &format!("Device wiped and formatted as {}.", args.fstype));
    Ok(())
}

/// Unmount a single partition so it can be formatted or deleted without
/// leaving the app. The partition path comes straight from the layout.
pub fn unmount_partition(partition: String) -> Result<()> {
    disk::run_privileged("umount", &[&partition])?;
    Ok(())
}

// ── ISO architecture detection ────────────────────────────────────────────

/// What the Add ISO dialog needs to know about an ISO before it is added.
#[derive(Serialize, Deserialize)]
pub struct IsoInfo {
    /// "x86_64", "aarch64", or "unknown".
    pub arch: String,
    /// Unpacked root filesystem size in bytes; 0 if unknown.
    pub unpacked_bytes: u64,
    /// True when the size is the ISO's own record, false when estimated.
    pub unpacked_exact: bool,
}

/// Mount an ISO read-only and probe its architecture (kernel ELF header) and
/// unpacked root size. Called from ImagesStep before the ISO is added, to warn
/// about an incompatible architecture and to size the persistent partition.
pub fn get_iso_info(path: String) -> Result<IsoInfo> {
    let iso_mnt = disk::make_temp_dir()?;
    squash::mount_iso(&path, &iso_mnt)?;
    let arch = squash::detect_iso_arch(&iso_mnt).to_string();
    let (unpacked_bytes, unpacked_exact) = squash::unpacked_size(&iso_mnt).unwrap_or((0, false));
    cleanup_mount(&iso_mnt);
    Ok(IsoInfo { arch, unpacked_bytes, unpacked_exact })
}

// ── Theme detection ────────────────────────────────────────────────────────

// ══ Internal helpers ═══════════════════════════════════════════════════════

/// Mount points that could not be unmounted cleanly during this job. They are
/// detached lazily and reported as a warning when the job ends.
static LEFT_BUSY: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Unmount a temporary mount and remove its directory. A busy mount (a file
/// manager or other process reading it) is retried three times a second
/// apart, then detached lazily so the drive is not left mounted.
fn cleanup_mount(mnt: &str) {
    let mut unmounted = false;
    for attempt in 0..3 {
        if disk::run_privileged("umount", &[mnt]).is_ok() {
            unmounted = true;
            break;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    if !unmounted && is_mounted(mnt) {
        let _ = disk::run_privileged("umount", &["-l", mnt]);
        if let Ok(mut busy) = LEFT_BUSY.lock() {
            busy.push(mnt.to_string());
        }
    }
    let _ = std::fs::remove_dir(mnt);
}

fn is_mounted(mnt: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(mnt)))
        .unwrap_or(false)
}

/// Run `sync` while reporting how much written data is still waiting to
/// reach the drive. A slow stick can take minutes here with nothing else to
/// show, which looks like a hang.
fn sync_with_progress(p: &dyn Progress, stage: &str, pct: u8) -> Result<()> {
    let sync = std::thread::spawn(|| disk::run_privileged("sync", &[]).map(|_| ()));
    let mut last_shown: Option<u64> = None;
    let mut last_emit = std::time::Instant::now();
    while !sync.is_finished() {
        let mb = pending_write_mb();
        let due = last_emit.elapsed() >= std::time::Duration::from_secs(2);
        if mb > 0 && Some(mb) != last_shown && (last_shown.is_none() || due) {
            p.emit(stage, pct, &format!("Finishing write to drive — {mb} MB left…"));
            last_shown = Some(mb);
            last_emit = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    sync.join().map_err(|_| BootOsProError::Other("sync failed".into()))?
}

/// Data written but not yet on disk (Dirty + Writeback), in MB.
fn pending_write_mb() -> u64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb: u64 = meminfo.lines()
        .filter(|l| l.starts_with("Dirty:") || l.starts_with("Writeback:"))
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum();
    kb / 1000
}

fn find_efi(device: &str) -> Result<String> {
    let layout = disk::get_partition_layout(device)?;
    layout.children.iter()
        .find(|p| p.label.to_uppercase() == "ESP")
        .map(|p| p.path.clone())
        .ok_or_else(|| BootOsProError::Other("EFI partition not found".into()))
}

fn find_free_space(device: &str) -> Result<String> {
    let layout = disk::get_partition_layout(device)?;
    layout.children.iter()
        .find(|p| p.label.to_uppercase() == "FREESPACE")
        .map(|p| p.path.clone())
        .ok_or_else(|| BootOsProError::Other(
            "No free space partition found on this drive. The drive may be fully allocated.".into()
        ))
}

/// Extract a persistent ISO's squashfs into its partition, set up the user,
/// and return (kernel, initrd, fs_uuid). Shared by full write and add-persistent.
///
/// `base_pct`/`span_pct` band this ISO's extraction into the overall extract
/// stage: streamed unsquashfs progress (0–100) is mapped into
/// [base_pct, base_pct + span_pct] so multi-ISO writes never move backwards.
fn extract_persistent(
    p: &dyn Progress,
    iso_path: &str,
    part: &str,
    iso: &IsoWriteSpec,
    base_pct: u8,
    span_pct: u8,
) -> Result<(String, String, String)> {
    let iso_mnt = disk::make_temp_dir()?;
    let part_mnt = disk::make_temp_dir()?;

    squash::mount_iso(iso_path, &iso_mnt)?;

    let iso_filename = Path::new(iso_path)
        .file_name().and_then(|n| n.to_str()).unwrap_or("image.iso");

    // Detect extraction strategy by probing the ISO structure, not by filename.
    // This correctly handles Fedora/RHEL two-level LiveOS and all other families.
    let extraction_kind = match squash::probe_extraction(&iso_mnt) {
        Ok(k) => k,
        Err(e) => { cleanup_mount(&iso_mnt); let _ = std::fs::remove_dir(&part_mnt); return Err(e); }
    };

    if let Err(e) = disk::mount_partition_fast(part, &part_mnt) {
        cleanup_mount(&iso_mnt);
        let _ = std::fs::remove_dir(&part_mnt);
        return Err(e);
    }

    let kind_label = match &extraction_kind {
        squash::ExtractionKind::FedoraLiveOS(_) => "Fedora/RHEL LiveOS",
        squash::ExtractionKind::ErofsLiveOS(_) => "Fedora LiveOS (EROFS)",
        squash::ExtractionKind::SingleSquashfs(_) => "squashfs",
    };
    p.emit("extract", base_pct,
        &format!("Extracting {} filesystem for {}…", kind_label, iso.name));

    // Map streamed 0–100 unsquashfs progress into this ISO's band. Throttle to
    // whole-percent changes so the event channel isn't flooded.
    let iso_name = iso.name.clone();
    let mut last_emitted: u8 = 0;
    let mut on_progress = |pct: u8| {
        let banded = base_pct.saturating_add((pct as u16 * span_pct as u16 / 100) as u8);
        if banded != last_emitted {
            last_emitted = banded;
            // Constant message — the frontend de-duplicates consecutive
            // identical log lines, so streaming moves the bar without
            // flooding the log with one line per percent.
            p.emit("extract", banded, &format!("Extracting {iso_name}…"));
        }
    };

    if let Err(e) = squash::extract_iso(&extraction_kind, &part_mnt, &mut on_progress) {
        cleanup_mount(&iso_mnt);
        cleanup_mount(&part_mnt);
        // A killed extraction fails like any other — report it as the cancel it was.
        return Err(if is_cancelled() { BootOsProError::Cancelled } else { e });
    }

    // Detect kernel/initrd from the extracted content — never hardcode casper.
    let kernel = squash::find_kernel(&part_mnt, iso_filename)
        .unwrap_or_else(|| "/boot/vmlinuz".to_string());
    let initrd = squash::find_initrd(&part_mnt, iso_filename)
        .unwrap_or_else(|| "/boot/initrd.img".to_string());

    // Live images ship no root entry in /etc/fstab; an installed OS needs one.
    let fs_uuid = disk::partition_uuid(part);
    if fs_uuid.is_empty() {
        p.emit("config", 0, &format!(
            "Warning: no filesystem UUID for {}; /etc/fstab not written.", iso.name
        ));
    } else if let Err(e) = write_root_fstab(&part_mnt, &fs_uuid) {
        cleanup_mount(&iso_mnt);
        cleanup_mount(&part_mnt);
        return Err(BootOsProError::Other(format!(
            "Could not write /etc/fstab on {}: {e}", iso.name
        )));
    }

    // Create the user the operator specified in the UI.
    if !iso.username.is_empty() && !iso.password.is_empty() {
        p.emit("config", 0, &format!("Setting up user {}…", iso.username));
        // A persistent OS nobody can log in to is not a successful write.
        if let Err(e) = setup_user(&part_mnt, &iso.username, &iso.password, p) {
            cleanup_mount(&iso_mnt);
            cleanup_mount(&part_mnt);
            return Err(BootOsProError::Other(format!(
                "Could not create user {} on {}: {e}", iso.username, iso.name
            )));
        }
    }

    // Files written from this host (/etc/fstab; passwd, group, shadow and the
    // home folder from user setup) carry no SELinux label. On an enforcing
    // system (Fedora) D-Bus, login and the display manager then fail. Ask the
    // OS to relabel on first boot.
    if selinux_active(&part_mnt) {
        p.emit("config", 0, &format!(
            "{} uses SELinux: its first boot relabels files and restarts once.", iso.name
        ));
        if let Err(e) = std::fs::write(format!("{part_mnt}/.autorelabel"), b"") {
            cleanup_mount(&iso_mnt);
            cleanup_mount(&part_mnt);
            return Err(BootOsProError::Other(format!(
                "Could not mark {} for SELinux relabel: {e}", iso.name
            )));
        }
    }

    cleanup_mount(&iso_mnt);
    cleanup_mount(&part_mnt);

    Ok((manifest::sanitize_path(&kernel), manifest::sanitize_path(&initrd), fs_uuid))
}

/// Largest file FAT32 can hold (4 GiB - 1 byte).
const FAT32_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;

/// Copy one or more live ISOs onto an already-formatted free space partition
/// WITHOUT reformatting it. Detects each ISO's kernel/initrd by mounting it
/// read-only, and on exFAT builds the overlay a casper ISO needs to boot from
/// it. Returns the live records to add to the manifest. Every exit path
/// unmounts the partition.
fn copy_live_isos(
    p: &dyn Progress,
    free_part: &str,
    isos: &[LiveIsoSpec],
) -> Result<Vec<LiveRecord>> {
    let is_fat32 = disk::partition_fstype(free_part).eq_ignore_ascii_case("vfat");
    let free_mnt = disk::make_temp_dir()?;
    if let Err(e) = disk::mount_partition_fat(free_part, &free_mnt) {
        let _ = std::fs::remove_dir(&free_mnt);
        return Err(e);
    }
    let result = copy_live_isos_into(p, &free_mnt, is_fat32, isos);
    cleanup_mount(&free_mnt);
    result
}

fn copy_live_isos_into(
    p: &dyn Progress,
    free_mnt: &str,
    is_fat32: bool,
    isos: &[LiveIsoSpec],
) -> Result<Vec<LiveRecord>> {
    let mut records = Vec::new();

    for iso in isos {
        if is_cancelled() { return Err(BootOsProError::Cancelled); }

        let iso_size = std::fs::metadata(&iso.path)
            .map_err(|e| BootOsProError::Other(format!("stat iso: {e}")))?
            .len();

        if is_fat32 && iso_size > FAT32_MAX_FILE {
            return Err(BootOsProError::Other(format!(
                "{} is {:.1} GiB. The free space partition on this drive is FAT32, which \
                 cannot hold a file of 4 GiB or more. Use Clear Live in Disk Manager to \
                 re-format it as exFAT, then add the ISO again.",
                iso.name, iso_size as f64 / (1024.0 * 1024.0 * 1024.0)
            )));
        }

        let avail = fat_free_bytes(free_mnt);
        if iso_size > avail {
            return Err(BootOsProError::Other(format!(
                "{} ({} MB) does not fit in the remaining free space ({} MB).",
                iso.name, iso_size / 1_000_000, avail / 1_000_000
            )));
        }

        let filename = manifest::sanitize_filename(
            Path::new(&iso.path).file_name().and_then(|n| n.to_str()).unwrap_or("image.iso")
        );
        let dest = format!("{free_mnt}/{filename}");

        p.emit("copy", 0, &format!("Copying {}…", iso.name));
        if let Err(e) = copy_with_progress(Path::new(&iso.path), Path::new(&dest), &iso.name, p) {
            // Cancelled or failed mid-copy: drop the partial ISO.
            let _ = std::fs::remove_file(&dest);
            return Err(e);
        }

        // Detect family, kernel/initrd and boot params by probing the ISO's
        // actual structure and reading its real volume label. On exFAT a
        // casper ISO also gets its overlay initrd.
        let stem = filename.strip_suffix(".iso").unwrap_or(&filename);
        let overlay = manifest::sanitize_filename(&format!("{stem}.exfat.img"));
        let overlay_dir = format!("{free_mnt}/bootospro");
        let overlay_dest = (!is_fat32).then(|| format!("{overlay_dir}/{overlay}"));
        if overlay_dest.is_some() {
            let _ = std::fs::create_dir_all(&overlay_dir);
        }
        let (info, built) = match probe_live_iso(&iso.path, overlay_dest.as_deref(), p, &iso.name) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                return Err(e);
            }
        };

        records.push(LiveRecord {
            filename,
            os_name: iso.name.clone(),
            kernel: manifest::sanitize_path(&info.kernel),
            initrd: manifest::sanitize_path(&info.initrd),
            boot_params: manifest::sanitize_boot_params(&info.boot_params),
            locate: info.locate,
            size_bytes: iso_size,
            overlay: if built { overlay } else { String::new() },
        });
    }

    p.emit("copy", 100, "Live ISO(s) copied.");
    sync_with_progress(p, "copy", 100)?;
    Ok(records)
}

/// Mount a live ISO read-only, read its volume label, and probe its structure
/// to determine family, kernel, initrd, boot params and locate method. When
/// `overlay_dest` is set and the ISO is casper, build its exFAT overlay there.
/// Returns the boot info and whether an overlay was built.
fn probe_live_iso(
    iso_path: &str,
    overlay_dest: Option<&str>,
    p: &dyn Progress,
    name: &str,
) -> Result<(squash::LiveBootInfo, bool)> {
    let label = disk::iso_volume_label(iso_path);
    let iso_mnt = disk::make_temp_dir()?;
    if let Err(e) = squash::mount_iso(iso_path, &iso_mnt) {
        let _ = std::fs::remove_dir(&iso_mnt);
        return Err(e);
    }
    let info = squash::detect_live_boot(&iso_mnt, &label);
    let is_casper = info.kernel == "/casper/vmlinuz";
    let built = match overlay_dest {
        Some(dest) if is_casper => {
            p.emit("copy", 100, &format!("Preparing {name} to boot from exFAT…"));
            squash::build_casper_exfat_overlay(&iso_mnt, &info.initrd, dest).map(|_| true)
        }
        _ => Ok(false),
    };
    cleanup_mount(&iso_mnt);
    Ok((info, built?))
}

/// Free bytes on a mounted FAT partition via `df` (portable, no statvfs FFI).
fn fat_free_bytes(mnt: &str) -> u64 {
    let out = std::process::Command::new("df")
        .args(["--output=avail", "-B1", mnt])
        .output();
    if let Ok(o) = out {
        if let Some(line) = String::from_utf8_lossy(&o.stdout).lines().nth(1) {
            return line.trim().parse().unwrap_or(0);
        }
    }
    0
}

// ── Manifest ↔ GRUB projection ─────────────────────────────────────────────

fn persistent_entries_from_records(records: &[PersistentRecord]) -> Vec<PersistentEntry> {
    records.iter()
        .filter(|r| r.state == SlotState::Filled && !r.kernel.is_empty())
        .map(|r| PersistentEntry {
            name: r.os_name.clone().unwrap_or_else(|| r.label.clone()),
            label: r.label.clone(),
            // The manifest is untrusted input — a UUID is strictly hex and
            // dashes, so anything else is stripped before it reaches grub.cfg.
            fs_uuid: r.fs_uuid.chars()
                .filter(|c| c.is_ascii_hexdigit() || *c == '-')
                .take(36)
                .collect(),
            kernel: r.kernel.clone(),
            initrd: r.initrd.clone(),
            boot_params: r.boot_params.clone(),
        })
        .collect()
}

fn live_entries_from_records(records: &[LiveRecord], free_uuid: &str) -> Vec<LiveEntry> {
    // The manifest is untrusted input — a UUID is strictly hex and dashes.
    let free_uuid: String = free_uuid.chars()
        .filter(|c| c.is_ascii_hexdigit() || *c == '-')
        .take(36)
        .collect();
    records.iter()
        .map(|r| LiveEntry {
            name: r.os_name.clone(),
            filename: r.filename.clone(),
            kernel: r.kernel.clone(),
            initrd: r.initrd.clone(),
            boot_params: r.boot_params.clone(),
            // Drives written before 1.0.18 recorded Arch as "none"; their
            // entries are corrected on the next GRUB rewrite.
            locate: if r.locate == "none" && r.boot_params.contains("archisolabel=") {
                "archiso".into()
            } else {
                r.locate.clone()
            },
            free_uuid: free_uuid.clone(),
            overlay: r.overlay.clone(),
        })
        .collect()
}

/// Regenerate GRUB from the manifest, then write the manifest. Order matters:
/// GRUB first so a write failure does not leave a manifest describing a menu
/// that was never written.
fn rewrite_grub_and_manifest(efi_mnt: &str, m: &mut Manifest) -> Result<()> {
    let persistent = persistent_entries_from_records(&m.persistent);
    let live = live_entries_from_records(&m.live, &m.free_space.fs_uuid);
    grub::update_grub_config(efi_mnt, &persistent, &live)?;
    write_manifest(efi_mnt, m)
}

fn write_manifest(efi_mnt: &str, m: &Manifest) -> Result<()> {
    let dir = format!("{efi_mnt}/bootospro");
    let _ = disk::run_privileged("mkdir", &["-p", &dir]);

    manifest::write_to_efi(efi_mnt, m, |src, dest| {
        disk::run_privileged("cp", &[src, dest])
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .map_err(BootOsProError::Other)
}

/// Load the manifest from EFI, or rebuild it from the physical drive if absent
/// or corrupt. Guarantees a usable manifest for downstream edits.
fn load_or_rebuild_manifest(efi_mnt: &str, device: &str) -> Result<Manifest> {
    match manifest::read_from_efi(efi_mnt) {
        Ok(Some(m)) => Ok(m),
        _ => rebuild_manifest_from_drive(device, "BOOTOSPRO", app_version()),
    }
}

/// Reconstruct a manifest by probing the physical partitions. Boot params and
/// kernel paths cannot be recovered from a bare partition, so filled persistent
/// slots are recorded with best-effort defaults; GRUB is rebuilt from whatever
/// can be determined. This is the fallback, not the happy path.
fn rebuild_manifest_from_drive(device: &str, drive_label: &str, app_version: &str) -> Result<Manifest> {
    let layout = disk::get_partition_layout(device)?;
    let mut m = Manifest::new(drive_label, app_version);

    for p in &layout.children {
        let label_up = p.label.to_uppercase();
        if label_up == "ESP" || label_up == "BOOTOSPRO" || p.part_type.starts_with("21686148") {
            continue; // EFI, installer, BIOS boot — not OS slots
        }
        if label_up == "FREESPACE" {
            m.free_space = FreeSpaceRecord {
                label: "FREESPACE".into(),
                size_bytes: p.size_bytes,
                fs_uuid: disk::partition_uuid(&p.path),
            };
            continue;
        }
        if p.fstype == "ext4" && !p.label.is_empty() {
            let (kernel, initrd, filled) = probe_persistent(&p.path);
            m.persistent.push(PersistentRecord {
                label: manifest::sanitize_label(&p.label),
                os_name: if filled { Some(p.label.clone()) } else { None },
                kernel,
                initrd,
                boot_params: if filled { "quiet splash".into() } else { String::new() },
                size_bytes: p.size_bytes,
                fs_uuid: disk::partition_uuid(&p.path),
                state: if filled { SlotState::Filled } else { SlotState::Empty },
            });
        }
    }

    m.installer = InstallerRecord {
        present: layout.children.iter().any(|p| p.label.to_uppercase() == "BOOTOSPRO"),
        packages: Vec::new(),
    };
    Ok(m)
}

/// Mount an ext4 partition read-only and probe for a kernel to decide whether
/// the slot is filled. Returns (kernel, initrd, filled).
fn probe_persistent(part: &str) -> (String, String, bool) {
    let mnt = match disk::make_temp_dir() { Ok(m) => m, Err(_) => return (String::new(), String::new(), false) };
    if disk::run_privileged("mount", &["-o", "ro", part, &mnt]).is_err() {
        let _ = std::fs::remove_dir(&mnt);
        return (String::new(), String::new(), false);
    }
    let kernel = squash::find_kernel(&mnt, "");
    let initrd = squash::find_initrd(&mnt, "");
    cleanup_mount(&mnt);
    match (kernel, initrd) {
        (Some(k), Some(i)) => (k, i, true),
        _ => (String::new(), String::new(), false),
    }
}

// ── Progress copy ──────────────────────────────────────────────────────────

fn copy_with_progress(src: &Path, dest: &Path, name: &str, p: &dyn Progress) -> Result<()> {
    use std::io::{Read, Write};

    let total = std::fs::metadata(src)?.len().max(1);
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::File::create(dest)?;

    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut written: u64 = 0;
    let mut last_pct: u8 = 0;

    loop {
        if is_cancelled() { return Err(BootOsProError::Cancelled); }
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        writer.write_all(&buf[..n])?;
        written += n as u64;

        let pct = (written * 100 / total) as u8;
        if pct != last_pct {
            last_pct = pct;
            p.emit("copy", pct,
                &format!("Copying {name}… {}/{} MB", written / 1_000_000, total / 1_000_000));
        }
    }
    Ok(())
}

// ── User setup ─────────────────────────────────────────────────────────────

/// Usernames accepted by useradd on every supported family: a lowercase
/// letter first, then lowercase letters, digits, `_` or `-`, at most 32.
/// This also keeps `:` and line breaks out of the chpasswd input.
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The extracted OS's /etc/fstab with a root line for `uuid` added. Existing
/// lines are kept. None when the file already mounts `/`.
fn fstab_with_root(existing: &str, uuid: &str) -> Option<String> {
    let has_root = existing.lines().any(|l| {
        let l = l.trim();
        !l.starts_with('#') && l.split_whitespace().nth(1) == Some("/")
    });
    if has_root {
        return None;
    }
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("UUID={uuid} / ext4 defaults 0 1\n"));
    Some(out)
}

/// Add the root filesystem to the extracted OS's /etc/fstab.
fn write_root_fstab(part_mnt: &str, uuid: &str) -> std::io::Result<()> {
    let path = format!("{part_mnt}/etc/fstab");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match fstab_with_root(&existing, uuid) {
        Some(new) => std::fs::write(&path, new),
        None => Ok(()),
    }
}

/// True when the extracted OS runs SELinux (enforcing or permissive).
fn selinux_active(part_mnt: &str) -> bool {
    std::fs::read_to_string(format!("{part_mnt}/etc/selinux/config"))
        .map(|c| c.lines().any(|l| {
            let l = l.trim();
            l == "SELINUX=enforcing" || l == "SELINUX=permissive"
        }))
        .unwrap_or(false)
}

/// The extracted OS's admin group: `sudo` (Debian/Ubuntu family) or `wheel`
/// (Fedora/RHEL/Arch). None if it has neither.
fn admin_group(part_mnt: &str) -> Option<&'static str> {
    let group = std::fs::read_to_string(format!("{part_mnt}/etc/group")).unwrap_or_default();
    let has = |g: &str| group.lines().any(|l| l.starts_with(&format!("{g}:")));
    ["sudo", "wheel"].into_iter().find(|g| has(g))
}

/// Create a user with password in the extracted filesystem via chroot.
/// The credential file is written INSIDE the chroot so chpasswd can read it.
fn setup_user(part_mnt: &str, username: &str, password: &str, p: &dyn Progress) -> Result<()> {
    if !valid_username(username) {
        return Err(BootOsProError::Other(format!(
            "invalid username '{username}' — use lowercase letters, digits, _ or -, starting with a letter"
        )));
    }
    if password.contains('\n') || password.contains('\r') {
        return Err(BootOsProError::Other("password must not contain line breaks".into()));
    }

    let group = admin_group(part_mnt);
    if group.is_none() {
        p.emit("config", 0, "No sudo or wheel group in this OS — user created without admin rights.");
    }

    let passwd_path = format!("{part_mnt}/etc/passwd");
    let passwd = std::fs::read_to_string(&passwd_path).unwrap_or_default();
    let user_exists = passwd.lines().any(|l| l.starts_with(&format!("{username}:")));

    if !user_exists {
        let mut args = vec![part_mnt, "useradd", "-m", "-s", "/bin/bash"];
        if let Some(g) = group {
            args.extend_from_slice(&["-G", g]);
        }
        args.push(username);
        disk::run_privileged("chroot", &args)?;
    } else if let Some(g) = group {
        disk::run_privileged("chroot", &[part_mnt, "usermod", "-aG", g, username])?;
    }

    // If useradd -m didn't create the home dir (pre-existing user without
    // one), create it and chown to the user's REAL uid:gid parsed from the
    // chroot's passwd, not an assumed 1000:1000 (the extracted OS may already
    // own uid 1000).
    let home = format!("{part_mnt}/home/{username}");
    if !std::path::Path::new(&home).exists() {
        disk::run_privileged("mkdir", &["-p", &home])?;
        let passwd = std::fs::read_to_string(&passwd_path).unwrap_or_default();
        if let Some((uid, gid)) = passwd.lines()
            .find(|l| l.starts_with(&format!("{username}:")))
            .and_then(|l| {
                let f: Vec<&str> = l.split(':').collect();
                Some((f.get(2)?.to_string(), f.get(3)?.to_string()))
            })
            .filter(|(u, g)| {
                u.chars().all(|c| c.is_ascii_digit()) && g.chars().all(|c| c.is_ascii_digit())
            })
        {
            disk::run_privileged("chown", &[&format!("{uid}:{gid}"), &home])?;
        }
        disk::run_privileged("chmod", &["755", &home])?;
    }

    // Credential file inside the chroot: host {part_mnt}/tmp/.. = chroot /tmp/..
    // Owner-only (0600) for the moment it exists.
    let cred_host = format!("{part_mnt}/tmp/bootospro-pw");
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600)
            .open(&cred_host)
            .map_err(|e| BootOsProError::Other(format!("write chpasswd file: {e}")))?;
        writeln!(f, "{username}:{password}")
            .map_err(|e| BootOsProError::Other(format!("write chpasswd file: {e}")))?;
    }

    let result = disk::run_privileged("chroot", &[
        part_mnt, "sh", "-c", "chpasswd < /tmp/bootospro-pw",
    ]);
    let _ = std::fs::remove_file(&cred_host);
    result?;

    Ok(())
}

// ══ Job protocol ════════════════════════════════════════════════════════════

/// One privileged operation. The app sends exactly one per helper launch.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Job {
    FullWrite(FullWriteArgs),
    AddPersistent(AddPersistentArgs),
    AddLive(AddLiveArgs),
    FillSlot(FillSlotArgs),
    FormatPersistentSlot(FormatPersistentArgs),
    FormatFreeSpaceSlot(FormatFreeArgs),
    ReadDriveManifest { device: String },
    ReconcileManifest { device: String, drive_label: String },
    FormatPartition(FormatPartitionArgs),
    DeletePartition(DeletePartitionArgs),
    WipeDevice(WipeDeviceArgs),
    UnmountPartition { partition: String },
    GetIsoInfo { path: String },
}

/// The single line the app writes to the helper's stdin.
#[derive(Serialize, Deserialize)]
pub struct JobRequest {
    /// App version, recorded in manifests (the helper has no AppHandle).
    pub app_version: String,
    pub job: Job,
}

/// Stop the running job: set the flag checked between steps and stop the
/// running extraction. The job then unwinds through its cleanup paths.
pub fn request_cancel() {
    set_cancelled(true);
    disk::kill_running();
}

/// The app's real version for this job (from the JobRequest). Cargo.toml's
/// version is never stamped by build.sh, so CARGO_PKG_VERSION is not used.
static APP_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn app_version() -> &'static str {
    APP_VERSION.get().map(String::as_str).unwrap_or("unknown")
}

/// Validate and run one job. Returns the job's result as JSON (null for jobs
/// that return nothing).
pub fn run(req: JobRequest, p: &dyn Progress) -> Result<serde_json::Value> {
    validate(&req.job)?;
    set_cancelled(false);
    let _ = APP_VERSION.set(req.app_version.clone());
    let v = req.app_version.as_str();
    let to_json = |r: std::result::Result<serde_json::Value, serde_json::Error>| {
        r.map_err(|e| BootOsProError::Other(format!("encode result: {e}")))
    };
    let value = run_job(req.job, v, p, to_json)?;
    let busy = LEFT_BUSY.lock().map(|b| b.clone()).unwrap_or_default();
    for mnt in busy {
        p.emit("sync", 100, &format!(
            "Warning: {mnt} was busy and could not be unmounted cleanly; it was \
             detached. Wait for the drive light to stop before removing the drive."
        ));
    }
    Ok(value)
}

fn run_job(
    job: Job,
    v: &str,
    p: &dyn Progress,
    to_json: impl Fn(std::result::Result<serde_json::Value, serde_json::Error>) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    Ok(match job {
        Job::FullWrite(a) => { run_full_write(a, v, p)?; serde_json::Value::Null }
        Job::AddPersistent(a) => { add_persistent_iso(a, p)?; serde_json::Value::Null }
        Job::AddLive(a) => { add_live_iso(a, p)?; serde_json::Value::Null }
        Job::FillSlot(a) => { fill_slot(a, p)?; serde_json::Value::Null }
        Job::FormatPersistentSlot(a) => { format_persistent_slot(a, p)?; serde_json::Value::Null }
        Job::FormatFreeSpaceSlot(a) => { format_free_space_slot(a, p)?; serde_json::Value::Null }
        Job::ReadDriveManifest { device } => to_json(serde_json::to_value(read_drive_manifest(device)?))?,
        Job::ReconcileManifest { device, drive_label } => {
            reconcile_manifest(device, drive_label, v)?;
            serde_json::Value::Null
        }
        Job::FormatPartition(a) => { format_partition(a, p)?; serde_json::Value::Null }
        Job::DeletePartition(a) => { delete_partition(a, p)?; serde_json::Value::Null }
        Job::WipeDevice(a) => { wipe_device(a, p)?; serde_json::Value::Null }
        Job::UnmountPartition { partition } => { unmount_partition(partition)?; serde_json::Value::Null }
        Job::GetIsoInfo { path } => to_json(serde_json::to_value(get_iso_info(path)?))?,
    })
}

// ── Validation ─────────────────────────────────────────────────────────────
//
// The helper runs as root, so it trusts nothing in the job: every device must
// be a USB drive the app itself would list, must not hold the running system,
// and every partition must belong to such a drive. ISOs must be regular files.

const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/home", "/usr", "/var"];

fn validate(job: &Job) -> Result<()> {
    let usb = disk::list_usb_devices()?;

    let check_dev = |dev: &str| -> Result<()> {
        let d = usb.iter().find(|u| u.path == dev).ok_or_else(|| {
            BootOsProError::Other(format!("refused: {dev} is not a USB drive"))
        })?;
        let holds_system = d.children.iter().any(|c| {
            c.mountpoint.as_deref().is_some_and(|m| SYSTEM_MOUNTS.contains(&m))
        });
        if holds_system {
            return Err(BootOsProError::Other(format!(
                "refused: {dev} holds the running system"
            )));
        }
        Ok(())
    };
    let check_part_of = |dev: &str, part: &str| -> Result<()> {
        check_dev(dev)?;
        let owned = usb.iter()
            .any(|u| u.path == dev && u.children.iter().any(|c| c.path == part));
        if owned { Ok(()) } else {
            Err(BootOsProError::Other(format!("refused: {part} is not a partition of {dev}")))
        }
    };
    let check_part = |part: &str| -> Result<()> {
        let parent = usb.iter()
            .find(|u| u.children.iter().any(|c| c.path == part))
            .ok_or_else(|| BootOsProError::Other(format!(
                "refused: {part} is not a partition of a USB drive"
            )))?;
        check_dev(&parent.path)
    };
    let check_iso = |path: &str| -> Result<()> {
        let p = Path::new(path);
        let ext_ok = p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("iso") || e.eq_ignore_ascii_case("img"));
        if p.is_file() && ext_ok { Ok(()) } else {
            Err(BootOsProError::Other(format!("refused: {path} is not an ISO image file")))
        }
    };

    match job {
        Job::FullWrite(a) => {
            check_dev(&a.device)?;
            for i in &a.isos { check_iso(&i.path)?; }
            for l in &a.live_isos { check_iso(&l.path)?; }
        }
        Job::AddPersistent(a) => { check_dev(&a.device)?; check_iso(&a.iso.path)?; }
        Job::AddLive(a) => { check_dev(&a.device)?; check_iso(&a.iso.path)?; }
        Job::FillSlot(a) => { check_dev(&a.device)?; check_iso(&a.iso.path)?; }
        Job::FormatPersistentSlot(a) => check_part_of(&a.device, &a.partition)?,
        Job::FormatFreeSpaceSlot(a) => check_dev(&a.device)?,
        Job::ReadDriveManifest { device } | Job::ReconcileManifest { device, .. } => {
            check_dev(device)?
        }
        Job::FormatPartition(a) => check_part(&a.partition)?,
        Job::DeletePartition(a) => check_part_of(&a.device, &a.partition)?,
        Job::WipeDevice(a) => check_dev(&a.device)?,
        Job::UnmountPartition { partition } => check_part(partition)?,
        Job::GetIsoInfo { path } => check_iso(path)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fstab_with_root;

    const U: &str = "285ec8ea-8302-47fc-bbfb-88160a73f346";
    const ROOT: &str = "UUID=285ec8ea-8302-47fc-bbfb-88160a73f346 / ext4 defaults 0 1\n";

    /// Fedora ships no fstab, Debian an empty one: just the root line.
    #[test]
    fn fstab_missing_or_empty_gets_root_line() {
        assert_eq!(fstab_with_root("", U).as_deref(), Some(ROOT));
    }

    /// Mint and Arch ship comments only: kept, root line added below.
    #[test]
    fn fstab_comments_kept() {
        let mint = "# UNCONFIGURED FSTAB FOR BASE SYSTEM";
        assert_eq!(
            fstab_with_root(mint, U),
            Some(format!("{mint}\n{ROOT}"))
        );
        let arch = "# Static information about the filesystems.\n# <file system> <dir> <type> <options> <dump> <pass>\n";
        assert_eq!(fstab_with_root(arch, U), Some(format!("{arch}{ROOT}")));
    }

    /// An fstab that already mounts / is left alone; a commented one is not counted.
    #[test]
    fn fstab_existing_root_untouched() {
        assert_eq!(fstab_with_root("UUID=abc / btrfs subvol=root 0 0\n", U), None);
        assert!(fstab_with_root("# UUID=abc / ext4 defaults 0 1\n", U).is_some());
    }
}
