//! Boot OS Pro drive manifest.
//!
//! The manifest is the master record of everything on a Boot OS Pro drive:
//! persistent OS partitions, live-session ISOs in the free space partition,
//! the installer payload, and free space. It lives as a plain JSON file on the
//! EFI partition alongside the GRUB config (`/bootospro/manifest.json`), and is
//! rewritten in the same operation that rewrites GRUB so the two never drift.
//!
//! Integrity model (two independent layers):
//!   1. Manifest self-hash — a SHA-256 of the manifest body, stored in the
//!      manifest itself. Detects a corrupted or truncated manifest. If the body
//!      hash does not validate, the manifest is treated as absent and the caller
//!      falls back to a live partition probe.
//!   2. Per-partition filesystem UUID — every `mkfs` generates a fresh UUID, so
//!      comparing the manifest's recorded UUID against the live `blkid` UUID
//!      tells us whether a partition was reformatted outside the app (e.g. by
//!      GParted). This is how drift is detected on drive selection.
//!
//! SECURITY: the manifest is treated as UNTRUSTED INPUT even though the app
//! wrote it, because anyone can mount the EFI partition and edit it, or the
//! drive may have come from another machine. No manifest value is ever
//! interpolated into a privileged shell string. Values that feed privileged
//! commands (labels, filenames, boot params) are sanitised on read — see
//! `sanitize_label`, `sanitize_filename`, `sanitize_boot_params`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Manifest schema version. Bump when the shape changes. Pre-deployment this
/// can move freely; once drives are in the wild a migration path is required.
pub const SCHEMA_VERSION: u32 = 1;

/// Relative path of the manifest on the EFI partition (under the mount point).
pub const MANIFEST_REL_PATH: &str = "bootospro/manifest.json";

// ── Manifest structures ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    /// App version that last wrote the manifest.
    pub app_version: String,
    /// ISO-8601 timestamp of the last write.
    pub updated: String,
    /// User-facing drive label.
    pub drive_label: String,

    pub persistent: Vec<PersistentRecord>,
    pub live: Vec<LiveRecord>,
    pub free_space: FreeSpaceRecord,
    pub installer: InstallerRecord,

    /// SHA-256 of the manifest body with this field blanked. Validated on read.
    #[serde(default)]
    pub body_hash: String,
}

/// One persistent OS partition (its own ext4, boots directly by label).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentRecord {
    /// Partition filesystem label, e.g. `MINT223`. Also the GRUB search key.
    pub label: String,
    /// Human-facing OS name, e.g. "Linux Mint 22.3". None when the slot is
    /// formatted but unfilled — rendered as "Persistent Empty" in the UI.
    pub os_name: Option<String>,
    /// Kernel path inside the partition, e.g. `/casper/vmlinuz`. Empty when empty.
    pub kernel: String,
    /// Initrd path(s) inside the partition. Empty when empty.
    pub initrd: String,
    /// Kernel boot parameters.
    pub boot_params: String,
    /// Partition size in bytes (informational / drift check).
    pub size_bytes: u64,
    /// Filesystem UUID captured at format time — primary drift signal.
    pub fs_uuid: String,
    /// Slot state.
    pub state: SlotState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlotState {
    /// ext4 partition with an OS extracted into it.
    Filled,
    /// ext4 partition formatted but no OS — available for a future install.
    Empty,
}

/// One live-session ISO living as a file on the shared free space partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveRecord {
    /// Bare filename on the free space partition, e.g. `sparky.iso`.
    pub filename: String,
    /// Human-facing name, e.g. "SparkyLinux 7".
    pub os_name: String,
    /// Kernel path inside the ISO, detected at write time, e.g. `/live/vmlinuz`.
    pub kernel: String,
    /// Initrd path(s) inside the ISO, e.g. `/live/initrd.img`.
    pub initrd: String,
    /// Kernel boot parameters, e.g. `boot=live components quiet splash`.
    pub boot_params: String,
    /// How GRUB locates the ISO on the FAT partition: "iso-scan", "findiso",
    /// or "none" (family uses its own label-based param in boot_params).
    #[serde(default)]
    pub locate: String,
    /// ISO file size in bytes.
    pub size_bytes: u64,
}

/// The shared FAT32 free space partition that holds the live ISOs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreeSpaceRecord {
    /// Partition label, always `FREESPACE`. Empty string if no free space part.
    pub label: String,
    /// Total partition size in bytes. 0 if no free space partition exists.
    pub size_bytes: u64,
    /// Filesystem UUID of the free space partition (drift signal).
    pub fs_uuid: String,
}

/// The 40 MB cross-platform installer partition (`BOOTOSPRO`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerRecord {
    /// Whether an installer partition exists on the drive.
    pub present: bool,
    /// Installer package files currently on the partition.
    pub packages: Vec<InstallerPackage>,
}

/// One installer package on the installer partition. Never bootable — catalog
/// only. Lets the app, on any machine, report what it can install from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallerPackage {
    /// Platform tag, e.g. "linux-deb", "linux-rpm", "windows", "macos".
    pub platform: String,
    /// Bare filename, e.g. `bootospro_0.1.24_amd64.deb`.
    pub filename: String,
    /// App version the package installs.
    pub version: String,
    /// SHA-256 of the package file (detects out-of-app replacement).
    pub sha256: String,
}

impl Manifest {
    /// A fresh, empty manifest for a brand new drive.
    pub fn new(drive_label: &str, app_version: &str) -> Self {
        Manifest {
            schema_version: SCHEMA_VERSION,
            app_version: app_version.to_string(),
            updated: now_iso8601(),
            drive_label: drive_label.to_string(),
            persistent: Vec::new(),
            live: Vec::new(),
            free_space: FreeSpaceRecord {
                label: String::new(),
                size_bytes: 0,
                fs_uuid: String::new(),
            },
            installer: InstallerRecord {
                present: false,
                packages: Vec::new(),
            },
            body_hash: String::new(),
        }
    }
}

// ── Read / write ────────────────────────────────────────────────────────────

/// Compute the body hash: serialise with body_hash blanked, hash the bytes.
/// Uses the system `sha256sum` (coreutils, present everywhere) to avoid adding
/// a hashing crate. Returns an empty string on failure — a blank stored hash
/// will then fail validation on read, which is the safe direction (treats the
/// manifest as untrusted rather than trusting an unverifiable one).
fn compute_body_hash(m: &Manifest) -> String {
    let mut clone = m.clone();
    clone.body_hash = String::new();
    let body = serde_json::to_vec(&clone).unwrap_or_default();

    let mut child = match Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(&body).is_err() {
            return String::new();
        }
    }

    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return String::new(),
    };

    // sha256sum prints "<hex>  -"; take the hex field.
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Serialise a manifest to pretty JSON with a freshly computed body hash.
/// Always refreshes `updated` and `body_hash` so callers don't have to.
pub fn serialize(m: &Manifest) -> Result<String, String> {
    let mut out = m.clone();
    out.updated = now_iso8601();
    out.body_hash = String::new();
    out.body_hash = compute_body_hash(&out);
    serde_json::to_string_pretty(&out).map_err(|e| format!("serialize manifest: {e}"))
}

/// Parse a manifest from JSON text and validate its self-hash.
/// Returns Err if the JSON is malformed OR the body hash does not match —
/// in both cases the caller should treat the drive as having no usable manifest.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let m: Manifest =
        serde_json::from_str(text).map_err(|e| format!("parse manifest: {e}"))?;

    let expected = m.body_hash.clone();
    let actual = compute_body_hash(&m);
    if expected != actual {
        return Err(format!(
            "manifest body hash mismatch (expected {expected}, got {actual}) — manifest corrupt or hand-edited"
        ));
    }
    Ok(m)
}

/// Read and validate the manifest from a mounted EFI partition.
/// `efi_mnt` is the EFI mount point. Returns Ok(None) if no manifest file is
/// present (a non-Boot-OS-Pro drive, or one created before manifests existed).
pub fn read_from_efi(efi_mnt: &str) -> Result<Option<Manifest>, String> {
    let path = format!("{efi_mnt}/{MANIFEST_REL_PATH}");
    if !Path::new(&path).exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read manifest: {e}"))?;
    parse(&text).map(Some)
}

/// Write the manifest to a temp file then move it into place via the privileged
/// helper, mirroring how grub.rs writes the GRUB config (grub-install creates
/// the EFI dirs as root, so a direct user write fails). The caller passes a
/// closure that performs the privileged copy so this module stays free of any
/// direct pkexec dependency.
pub fn write_to_efi<F>(efi_mnt: &str, m: &Manifest, privileged_cp: F) -> Result<(), String>
where
    F: Fn(&str, &str) -> Result<(), String>,
{
    let json = serialize(m)?;

    let pid = std::process::id();
    let tmp = format!("/tmp/bootospro-manifest-{pid}.json");
    std::fs::write(&tmp, &json).map_err(|e| format!("write tmp manifest: {e}"))?;

    // Ensure the bootospro/ dir exists on EFI, then copy the file in.
    let dest_dir = format!("{efi_mnt}/bootospro");
    let dest = format!("{efi_mnt}/{MANIFEST_REL_PATH}");

    // mkdir is part of the cp closure's responsibility via the helper; we hand
    // it both the source and destination and let the caller route privilege.
    let _ = std::fs::create_dir_all(&dest_dir); // best-effort if user-writable
    let result = privileged_cp(&tmp, &dest);
    let _ = std::fs::remove_file(&tmp);
    result
}

// ── Sanitisation (manifest is untrusted input) ──────────────────────────────

/// Filesystem labels: uppercase alphanumerics only, max 11 chars (FAT limit).
/// Anything else is stripped. Never allows characters meaningful to a shell.
pub fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .take(11)
        .collect()
}

/// Filenames: a bare basename only. Strips any path separators and any
/// character that could break out of a path or into a shell. Keeps dots and
/// hyphens so normal ISO/package names survive.
pub fn sanitize_filename(raw: &str) -> String {
    // Take only the final path component, then whitelist.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .take(255)
        .collect()
}

/// Boot parameters: whitelist the characters that legitimately appear in kernel
/// command lines. Blocks shell metacharacters (;, |, &, $, backtick, etc.) so a
/// hand-edited manifest cannot inject a command when params reach a boot config.
pub fn sanitize_boot_params(raw: &str) -> String {
    raw.chars()
        .filter(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, ' ' | '=' | '/' | '.' | '-' | '_' | ':' | ',')
        })
        .take(512)
        .collect()
}

/// Kernel / initrd paths inside an image: absolute-ish path characters only.
pub fn sanitize_path(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
        .take(255)
        .collect()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Minimal ISO-8601 UTC timestamp without pulling in chrono.
/// Format: YYYY-MM-DDТHH:MM:SSZ derived from the system clock.
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Civil-date conversion from Unix seconds (days since 1970), Howard Hinnant's
    // algorithm. Avoids a chrono dependency for a single timestamp.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}
