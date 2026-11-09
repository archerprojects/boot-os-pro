// Boot OS Pro — commands.rs — build 1.0.2 — Monday 9 November 2026
//
// Tauri commands. Read-only queries run here in the app; every privileged
// operation is a job run by the privileged helper (see jobs.rs and
// helper_client.rs) — one pkexec launch, one password prompt per operation.

use crate::disk::{self, BlockDevice};
use crate::error::{BootOsProError, Result};
use crate::helper_client;
use crate::jobs::{
    AddLiveArgs, AddPersistentArgs, DeletePartitionArgs, DriveReport, FillSlotArgs,
    FormatFreeArgs, FormatPartitionArgs, FormatPersistentArgs, FullWriteArgs, IsoInfo, Job,
    WipeDeviceArgs,
};
use tauri::AppHandle;

/// Run a job that returns nothing.
fn run(app: &AppHandle, job: Job) -> Result<()> {
    helper_client::run_job(app, job).map(|_| ())
}

// ── Device discovery (unprivileged) ────────────────────────────────────────

#[tauri::command]
pub async fn list_devices() -> Result<Vec<BlockDevice>> {
    disk::list_usb_devices()
}

#[tauri::command]
pub async fn get_partition_layout(device: String) -> Result<BlockDevice> {
    disk::get_partition_layout(&device)
}

#[tauri::command]
pub async fn get_file_size(path: String) -> Result<u64> {
    let meta = std::fs::metadata(&path)
        .map_err(|e| BootOsProError::Other(format!("stat {path}: {e}")))?;
    Ok(meta.len())
}

/// The Boot OS Pro version dpkg has installed, or "" if it cannot be read.
/// Compared with the running version before a write starts.
#[tauri::command]
pub async fn installed_version() -> String {
    std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "bootospro"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[tauri::command]
pub async fn get_theme_colors() -> crate::theme::ThemeColors {
    crate::theme::get_theme_colors()
}

// ── Writes (privileged helper jobs) ────────────────────────────────────────

#[tauri::command]
pub async fn run_full_write(args: FullWriteArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::FullWrite(args))
}

#[tauri::command]
pub async fn add_persistent_iso(args: AddPersistentArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::AddPersistent(args))
}

#[tauri::command]
pub async fn fill_slot(args: FillSlotArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::FillSlot(args))
}

#[tauri::command]
pub async fn add_live_iso(args: AddLiveArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::AddLive(args))
}

#[tauri::command]
pub async fn format_persistent_slot(args: FormatPersistentArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::FormatPersistentSlot(args))
}

#[tauri::command]
pub async fn format_free_space_slot(args: FormatFreeArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::FormatFreeSpaceSlot(args))
}

// ── Manifest ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn read_drive_manifest(device: String, app: AppHandle) -> Result<DriveReport> {
    let v = helper_client::run_job(&app, Job::ReadDriveManifest { device })?;
    serde_json::from_value(v).map_err(|e| BootOsProError::Other(format!("decode drive report: {e}")))
}

#[tauri::command]
pub async fn reconcile_manifest(device: String, drive_label: String, app: AppHandle) -> Result<()> {
    run(&app, Job::ReconcileManifest { device, drive_label })
}

// ── Cancellation ───────────────────────────────────────────────────────────

/// Forward Cancel to the running helper. The write command itself returns
/// `Cancelled` once the helper has stopped and cleaned up.
#[tauri::command]
pub async fn cancel_operation() -> Result<()> {
    helper_client::cancel();
    Ok(())
}

// ── Disk Manager ───────────────────────────────────────────────────────────

#[tauri::command]
pub async fn format_partition(args: FormatPartitionArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::FormatPartition(args))
}

#[tauri::command]
pub async fn delete_partition(args: DeletePartitionArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::DeletePartition(args))
}

#[tauri::command]
pub async fn wipe_device(args: WipeDeviceArgs, app: AppHandle) -> Result<()> {
    run(&app, Job::WipeDevice(args))
}

#[tauri::command]
pub async fn unmount_partition(partition: String, app: AppHandle) -> Result<()> {
    run(&app, Job::UnmountPartition { partition })
}

// ── ISO probe (architecture, unpacked size) ────────────────────────────────

/// Mounting the ISO needs root, so this is a helper job too.
#[tauri::command]
pub async fn get_iso_info(path: String, app: AppHandle) -> Result<IsoInfo> {
    let v = helper_client::run_job(&app, Job::GetIsoInfo { path })?;
    serde_json::from_value(v).map_err(|e| BootOsProError::Other(format!("decode iso info: {e}")))
}
