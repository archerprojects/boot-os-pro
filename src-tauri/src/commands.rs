use crate::disk::{self, BlockDevice};
use crate::error::{BootOsProError, Result};
use crate::grub::{self, PersistentEntry, LiveEntry};
use crate::manifest::{
    self, Manifest, PersistentRecord, LiveRecord, FreeSpaceRecord,
    InstallerRecord, SlotState,
};
use crate::squash;
use std::path::Path;
use tauri::{AppHandle, Emitter};

// ── Cancellation flag ──────────────────────────────────────────────────────

static CANCELLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_cancelled(v: bool) {
    CANCELLED.store(v, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(std::sync::atomic::Ordering::SeqCst)
}

// ── Device discovery ───────────────────────────────────────────────────────

#[tauri::command]
pub async fn list_devices() -> Result<Vec<BlockDevice>> {
    disk::list_usb_devices()
}

// ── Input specs from the frontend ──────────────────────────────────────────

#[derive(serde::Deserialize)]
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
#[derive(serde::Deserialize)]
pub struct LiveIsoSpec {
    pub name: String,
    pub path: String,
}

#[derive(serde::Deserialize)]
pub struct FullWriteArgs {
    pub device: String,
    pub drive_label: String,
    pub isos: Vec<IsoWriteSpec>,
    pub free_space_gb: u64,
    pub live_isos: Vec<LiveIsoSpec>,
}

// ── Full first write pipeline ──────────────────────────────────────────────

#[tauri::command]
pub async fn run_full_write(args: FullWriteArgs, app: AppHandle) -> Result<()> {
    set_cancelled(false);

    let app_version = app.package_info().version.to_string();
    let iso_sizes: Vec<u64> = args.isos.iter().map(|i| i.size_gb).collect();

    // 1. Partition (includes the 40 MB BOOTOSPRO installer partition)
    emit(&app, "partition", 0, "Partitioning drive…");
    let parts = disk::partition_drive_full(&args.device, &iso_sizes, args.free_space_gb)?;
    emit(&app, "partition", 100, "Drive partitioned.");

    // 2. Format EFI, installer, free space, and each ISO partition
    emit(&app, "format", 0, "Formatting EFI partition…");
    disk::format_efi(&parts.efi_part)?;

    emit(&app, "format", 15, "Formatting installer partition…");
    disk::format_installer(&parts.installer_part)?;

    if let Some(ref fp) = parts.free_part {
        emit(&app, "format", 30, "Formatting free space partition…");
        disk::format_free_space(fp)?;
    }

    emit(&app, "format", 50, "Formatting OS partitions…");
    for (i, iso) in args.isos.iter().enumerate() {
        if is_cancelled() { return Err(BootOsProError::Cancelled); }
        disk::format_iso_partition(&parts.iso_parts[i], &iso.label)?;
    }
    emit(&app, "format", 100, "Partitions formatted.");

    // 3. Mount EFI and install GRUB
    emit(&app, "grub", 0, "Mounting EFI partition…");
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&parts.efi_part, &efi_mnt)?;

    emit(&app, "grub", 20, "Installing GRUB (EFI + BIOS)…");
    grub::install_grub(&efi_mnt, &args.device)?;
    emit(&app, "grub", 100, "GRUB installed.");

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

            emit(&app, "extract", base_pct,
                &format!("Extracting {} ({}/{})…", iso.name, i + 1, total));

            let (kernel, initrd, fs_uuid) = extract_persistent(
                &app, &iso.path, &parts.iso_parts[i], iso, base_pct, span_pct,
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

            emit(&app, "extract", base_pct.saturating_add(span_pct),
                &format!("{} extracted.", iso.name));
        }

        // 5. Copy live ISOs onto the free space partition
        let mut live_records: Vec<LiveRecord> = Vec::new();
        let free_uuid = if let Some(ref fp) = parts.free_part {
            if !args.live_isos.is_empty() {
                let recs = copy_live_isos(&app, fp, &args.live_isos)?;
                live_records.extend(recs);
            }
            disk::partition_uuid(fp)
        } else {
            String::new()
        };

        // 6. Write GRUB config from the records
        emit(&app, "config", 0, "Writing GRUB configuration…");
        let persistent_entries = persistent_entries_from_records(&persistent_records);
        let live_entries = live_entries_from_records(&live_records);
        grub::write_grub_config(&efi_mnt, &persistent_entries, &live_entries)?;

        // 7. Build and write the manifest (master record on EFI).
        //    free_space records the ACTUAL trimmed partition size from
        //    partitioning, never the user-requested GB — all carve and
        //    live-add ceilings downstream derive from this value.
        let mut m = Manifest::new(&args.drive_label, &app_version);
        m.persistent = persistent_records;
        m.live = live_records;
        m.free_space = FreeSpaceRecord {
            label: if parts.free_part.is_some() { "FREESPACE".into() } else { String::new() },
            size_bytes: parts.free_space_bytes,
            fs_uuid: free_uuid,
        };
        m.installer = InstallerRecord { present: true, packages: Vec::new() };
        write_manifest(&efi_mnt, &m)?;
        emit(&app, "config", 100, "Configuration written.");

        // 8. Sync
        emit(&app, "sync", 0, "Syncing to disk…");
        disk::run_privileged("sync", &[])?;
        Ok(())
    };

    let result = body();
    cleanup_mount(&efi_mnt);
    result?;

    emit(&app, "sync", 100, "Done! USB drive is ready.");
    Ok(())
}

// ── Add persistent ISO to existing drive ──────────────────────────────────

#[derive(serde::Deserialize)]
pub struct AddPersistentArgs {
    pub device: String,
    pub iso: IsoWriteSpec,
}

#[tauri::command]
pub async fn add_persistent_iso(args: AddPersistentArgs, app: AppHandle) -> Result<()> {
    set_cancelled(false);

    emit(&app, "partition", 0, "Carving partition from free space…");
    let (new_part, _new_free) =
        disk::carve_persistent_from_free(&args.device, args.iso.size_gb)?;
    emit(&app, "partition", 100, "Partition created.");

    emit(&app, "format", 0, &format!("Formatting partition for {}…", args.iso.name));
    disk::format_iso_partition(&new_part, &args.iso.label)?;
    emit(&app, "format", 100, "Partition formatted.");

    emit(&app, "extract", 0, &format!("Extracting {}…", args.iso.name));
    let (kernel, initrd, fs_uuid) =
        extract_persistent(&app, &args.iso.path, &new_part, &args.iso, 0, 100)?;
    emit(&app, "extract", 100, &format!("{} extracted.", args.iso.name));

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

        emit(&app, "config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    emit(&app, "config", 100, "Drive updated.");

    Ok(())
}

// ── Add live ISO to existing drive (additive — no reformat) ────────────────

#[derive(serde::Deserialize)]
pub struct AddLiveArgs {
    pub device: String,
    pub iso: LiveIsoSpec,
}

#[tauri::command]
pub async fn add_live_iso(args: AddLiveArgs, app: AppHandle) -> Result<()> {
    set_cancelled(false);

    let free_part = find_free_space(&args.device)?;

    // Additive: copy the new ISO alongside any existing ones, do NOT reformat.
    let recs = copy_live_isos(&app, &free_part, std::slice::from_ref(&args.iso))?;

    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        for r in recs {
            m.live.retain(|l| l.filename != r.filename);
            m.live.push(r);
        }

        emit(&app, "config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    emit(&app, "config", 100, "Live ISO added.");

    Ok(())
}

// ── Scalpel: format one persistent partition in place ──────────────────────

#[derive(serde::Deserialize)]
pub struct FormatPersistentArgs {
    pub device: String,
    pub partition: String,
    pub label: String,
}

/// Format a single persistent partition, leaving all others intact. The slot
/// becomes "Persistent Empty" in the manifest and its GRUB entry is dropped.
#[tauri::command]
pub async fn format_persistent_slot(args: FormatPersistentArgs, app: AppHandle) -> Result<()> {
    // Unmount first — mkfs fails on a mounted partition (desktop automounts it).
    let _ = disk::run_privileged("umount", &[&args.partition]);
    emit(&app, "format", 0, &format!("Formatting {}…", args.partition));
    disk::format_iso_partition(&args.partition, &args.label)?;
    let fs_uuid = disk::partition_uuid(&args.partition);
    emit(&app, "format", 60, "Partition formatted.");

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

        emit(&app, "config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    emit(&app, "config", 100, "Slot is now empty and ready for a new OS.");

    Ok(())
}

// ── Format the free space partition (wipes ALL live ISOs at once) ──────────

#[derive(serde::Deserialize)]
pub struct FormatFreeArgs {
    pub device: String,
}

#[tauri::command]
pub async fn format_free_space_slot(args: FormatFreeArgs, app: AppHandle) -> Result<()> {
    let free_part = find_free_space(&args.device)?;

    // Unmount first — mkfs.fat fails on a mounted partition. The desktop
    // automounts removable partitions, so clear-live must handle that itself.
    let _ = disk::run_privileged("umount", &[&free_part]);

    emit(&app, "format", 0, "Formatting free space — all live ISOs will be removed…");
    disk::format_free_space(&free_part)?;
    let fs_uuid = disk::partition_uuid(&free_part);
    emit(&app, "format", 60, "Free space formatted.");

    let efi_part = find_efi(&args.device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let result = (|| -> Result<()> {
        let mut m = load_or_rebuild_manifest(&efi_mnt, &args.device)?;
        m.live.clear();
        m.free_space.fs_uuid = fs_uuid;

        emit(&app, "config", 0, "Updating configuration…");
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result?;
    emit(&app, "config", 100, "All live ISOs removed.");

    Ok(())
}

// ── Read the drive manifest (for the drive map / reconciliation) ───────────

#[derive(serde::Serialize)]
pub struct DriveReport {
    pub manifest: Option<Manifest>,
    /// True when the manifest does not match the physical drive (drift) and the
    /// frontend should offer to reconcile.
    pub drift: bool,
}

/// Read the manifest from a drive's EFI partition and check it against reality.
/// Drift = a recorded partition's filesystem UUID no longer matches (it was
/// reformatted outside the app, e.g. with GParted).
#[tauri::command]
pub async fn read_drive_manifest(device: String) -> Result<DriveReport> {
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
#[tauri::command]
pub async fn reconcile_manifest(device: String, drive_label: String, app: AppHandle) -> Result<()> {
    let efi_part = find_efi(&device)?;
    let efi_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(&efi_part, &efi_mnt)?;

    let app_version = app.package_info().version.to_string();
    let result = (|| -> Result<()> {
        let mut m = rebuild_manifest_from_drive(&device, &drive_label, &app_version)?;
        rewrite_grub_and_manifest(&efi_mnt, &mut m)?;
        disk::run_privileged("sync", &[])?;
        Ok(())
    })();
    cleanup_mount(&efi_mnt);
    result
}

// ── Cancellation ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn cancel_operation() -> Result<()> {
    set_cancelled(true);
    Ok(())
}

// ── Disk Manager ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_partition_layout(device: String) -> Result<disk::BlockDevice> {
    disk::get_partition_layout(&device)
}

#[derive(serde::Deserialize)]
pub struct FormatPartitionArgs {
    pub partition: String,
    pub fstype: String,
    pub label: String,
}

#[tauri::command]
pub async fn format_partition(args: FormatPartitionArgs, app: AppHandle) -> Result<()> {
    emit(&app, "format", 0, &format!("Formatting {} as {}…", args.partition, args.fstype));
    disk::format_partition(&args.partition, &args.fstype, &args.label)?;
    emit(&app, "format", 100, "Done.");
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct DeletePartitionArgs {
    pub device: String,
    pub partition: String,
}

#[tauri::command]
pub async fn delete_partition(args: DeletePartitionArgs, app: AppHandle) -> Result<()> {
    let part_num = disk::partition_number(&args.device, &args.partition)
        .ok_or_else(|| BootOsProError::Other(
            format!("could not parse partition number from {}", args.partition)))?;
    emit(&app, "partition", 0, &format!("Deleting partition {}…", part_num));
    disk::delete_partition(&args.device, part_num)?;
    emit(&app, "partition", 100, "Partition deleted.");
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct WipeDeviceArgs {
    pub device: String,
    pub label: String,
    pub fstype: String,
}

#[tauri::command]
pub async fn wipe_device(args: WipeDeviceArgs, app: AppHandle) -> Result<()> {
    emit(&app, "partition", 0, "Wiping device…");
    disk::wipe_device(&args.device, &args.label, &args.fstype)?;
    emit(&app, "partition", 100,
        &format!("Device wiped and formatted as {}.", args.fstype));
    Ok(())
}

/// Unmount a single partition so it can be formatted or deleted without
/// leaving the app. The partition path comes straight from the layout.
#[tauri::command]
pub async fn unmount_partition(partition: String) -> Result<()> {
    disk::run_privileged("umount", &[&partition])?;
    Ok(())
}

#[tauri::command]
pub async fn get_file_size(path: String) -> Result<u64> {
    let meta = std::fs::metadata(&path)
        .map_err(|e| BootOsProError::Other(format!("stat {path}: {e}")))?;
    Ok(meta.len())
}

// ── ISO architecture detection ────────────────────────────────────────────

/// Mount an ISO read-only, probe the kernel ELF header, return the arch string.
/// Returns "x86_64", "aarch64", or "unknown". Called from ImagesStep before
/// the ISO is added so the user can be warned about incompatible architecture.
#[tauri::command]
pub async fn get_iso_arch(path: String) -> Result<String> {
    let iso_mnt = disk::make_temp_dir()?;
    squash::mount_iso(&path, &iso_mnt)?;
    let arch = squash::detect_iso_arch(&iso_mnt);
    cleanup_mount(&iso_mnt);
    Ok(arch.to_string())
}

// ── Theme detection ────────────────────────────────────────────────────────

#[tauri::command]
pub async fn get_theme_colors() -> crate::theme::ThemeColors {
    crate::theme::get_theme_colors()
}

// ══ Internal helpers ═══════════════════════════════════════════════════════

fn emit(app: &AppHandle, stage: &str, pct: u8, msg: &str) {
    let _ = app.emit(
        "write_progress",
        serde_json::json!({ "stage": stage, "pct": pct, "msg": msg }),
    );
}

fn cleanup_mount(mnt: &str) {
    let _ = disk::run_privileged("umount", &[mnt]);
    let _ = std::fs::remove_dir(mnt);
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
    app: &AppHandle,
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

    disk::mount_partition_fast(part, &part_mnt)?;

    let kind_label = match &extraction_kind {
        squash::ExtractionKind::FedoraLiveOS(_) => "Fedora/RHEL LiveOS",
        squash::ExtractionKind::SingleSquashfs(_) => "squashfs",
    };
    emit(app, "extract", base_pct,
        &format!("Extracting {} filesystem for {}…", kind_label, iso.name));

    // Map streamed 0–100 unsquashfs progress into this ISO's band. Throttle to
    // whole-percent changes so the event channel isn't flooded.
    let iso_name = iso.name.clone();
    let mut last_emitted: u8 = 0;
    let mut on_progress = |p: u8| {
        let banded = base_pct.saturating_add((p as u16 * span_pct as u16 / 100) as u8);
        if banded != last_emitted {
            last_emitted = banded;
            // Constant message — the frontend de-duplicates consecutive
            // identical log lines, so streaming moves the bar without
            // flooding the log with one line per percent.
            emit(app, "extract", banded, &format!("Extracting {iso_name}…"));
        }
    };

    if let Err(e) = squash::extract_iso(&extraction_kind, &part_mnt, &mut on_progress) {
        cleanup_mount(&iso_mnt);
        cleanup_mount(&part_mnt);
        return Err(e);
    }

    // Detect kernel/initrd from the extracted content — never hardcode casper.
    let kernel = squash::find_kernel(&part_mnt, iso_filename)
        .unwrap_or_else(|| "/boot/vmlinuz".to_string());
    let initrd = squash::find_initrd(&part_mnt, iso_filename)
        .unwrap_or_else(|| "/boot/initrd.img".to_string());

    // Create the user the operator specified in the UI.
    if !iso.username.is_empty() && !iso.password.is_empty() {
        emit(app, "config", 0, &format!("Setting up user {}…", iso.username));
        if let Err(e) = setup_user(&part_mnt, &iso.username, &iso.password) {
            emit(app, "config", 0, &format!("Warning: user setup failed: {e}"));
        }
    }

    cleanup_mount(&iso_mnt);
    cleanup_mount(&part_mnt);

    let fs_uuid = disk::partition_uuid(part);
    Ok((manifest::sanitize_path(&kernel), manifest::sanitize_path(&initrd), fs_uuid))
}

/// Copy one or more live ISOs onto an already-formatted free space partition
/// WITHOUT reformatting it. Detects each ISO's kernel/initrd by mounting it
/// read-only. Returns the live records to add to the manifest.
fn copy_live_isos(
    app: &AppHandle,
    free_part: &str,
    isos: &[LiveIsoSpec],
) -> Result<Vec<LiveRecord>> {
    let free_mnt = disk::make_temp_dir()?;
    disk::mount_partition_fat(free_part, &free_mnt)?;

    let mut records = Vec::new();

    for iso in isos {
        if is_cancelled() { cleanup_mount(&free_mnt); return Err(BootOsProError::Cancelled); }

        let iso_size = std::fs::metadata(&iso.path)
            .map_err(|e| BootOsProError::Other(format!("stat iso: {e}")))?
            .len();

        let avail = fat_free_bytes(&free_mnt);
        if iso_size > avail {
            cleanup_mount(&free_mnt);
            return Err(BootOsProError::Other(format!(
                "{} ({} MB) does not fit in the remaining free space ({} MB).",
                iso.name, iso_size / 1_000_000, avail / 1_000_000
            )));
        }

        let filename = manifest::sanitize_filename(
            Path::new(&iso.path).file_name().and_then(|n| n.to_str()).unwrap_or("image.iso")
        );
        let dest = format!("{free_mnt}/{filename}");

        emit(app, "copy", 0, &format!("Copying {}…", iso.name));
        copy_with_progress(Path::new(&iso.path), Path::new(&dest), &iso.name, app)?;

        // Detect family, kernel/initrd and boot params by probing the ISO's
        // actual structure and reading its real volume label.
        let info = detect_live_boot(&iso.path)?;

        records.push(LiveRecord {
            filename,
            os_name: iso.name.clone(),
            kernel: manifest::sanitize_path(&info.kernel),
            initrd: manifest::sanitize_path(&info.initrd),
            boot_params: manifest::sanitize_boot_params(&info.boot_params),
            locate: info.locate,
            size_bytes: iso_size,
        });
    }

    disk::run_privileged("sync", &[])?;
    cleanup_mount(&free_mnt);
    emit(app, "copy", 100, "Live ISO(s) copied.");
    Ok(records)
}

/// Mount a live ISO read-only, read its volume label, and probe its structure
/// to determine family, kernel, initrd, boot params and locate method.
fn detect_live_boot(iso_path: &str) -> Result<squash::LiveBootInfo> {
    let label = disk::iso_volume_label(iso_path);
    let iso_mnt = disk::make_temp_dir()?;
    squash::mount_iso(iso_path, &iso_mnt)?;
    let info = squash::detect_live_boot(&iso_mnt, &label);
    cleanup_mount(&iso_mnt);
    Ok(info)
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

fn live_entries_from_records(records: &[LiveRecord]) -> Vec<LiveEntry> {
    records.iter()
        .map(|r| LiveEntry {
            name: r.os_name.clone(),
            filename: r.filename.clone(),
            kernel: r.kernel.clone(),
            initrd: r.initrd.clone(),
            boot_params: r.boot_params.clone(),
            locate: r.locate.clone(),
        })
        .collect()
}

/// Regenerate GRUB from the manifest, then write the manifest. Order matters:
/// GRUB first so a write failure does not leave a manifest describing a menu
/// that was never written.
fn rewrite_grub_and_manifest(efi_mnt: &str, m: &mut Manifest) -> Result<()> {
    let persistent = persistent_entries_from_records(&m.persistent);
    let live = live_entries_from_records(&m.live);
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
        _ => rebuild_manifest_from_drive(device, "BOOTOSPRO", env!("CARGO_PKG_VERSION")),
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

fn copy_with_progress(src: &Path, dest: &Path, name: &str, app: &AppHandle) -> Result<()> {
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
            emit(app, "copy", pct,
                &format!("Copying {name}… {}/{} MB", written / 1_000_000, total / 1_000_000));
        }
    }
    Ok(())
}

// ── User setup ─────────────────────────────────────────────────────────────

/// Create a user with password in the extracted filesystem via chroot.
/// The credential file is written INSIDE the chroot so chpasswd can read it.
fn setup_user(part_mnt: &str, username: &str, password: &str) -> Result<()> {
    let passwd_path = format!("{part_mnt}/etc/passwd");
    let passwd = std::fs::read_to_string(&passwd_path).unwrap_or_default();
    let user_exists = passwd.lines().any(|l| l.starts_with(&format!("{username}:")));

    if !user_exists {
        disk::run_privileged("chroot", &[
            part_mnt, "useradd", "-m", "-s", "/bin/bash", "-G", "sudo", username,
        ])?;
    }

    // If useradd -m didn't create the home dir (pre-existing user without
    // one), create it with allowlisted primitives — no `sh -c` composition —
    // and chown to the user's REAL uid:gid parsed from the chroot's passwd,
    // not an assumed 1000:1000 (the extracted OS may already own uid 1000).
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
    let cred_host = format!("{part_mnt}/tmp/bootospro-pw");
    std::fs::write(&cred_host, format!("{username}:{password}\n"))
        .map_err(|e| BootOsProError::Other(format!("write chpasswd file: {e}")))?;

    let result = disk::run_privileged("chroot", &[
        part_mnt, "sh", "-c", "chpasswd < /tmp/bootospro-pw",
    ]);
    let _ = std::fs::remove_file(&cred_host);
    result?;

    let _ = disk::run_privileged("chroot", &[
        part_mnt, "usermod", "-aG", "sudo", username,
    ]);

    Ok(())
}
