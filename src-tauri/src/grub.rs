use crate::error::{BootOsProError, Result};
use crate::disk::run_privileged;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// An ISO entry for a persistent partition — boots directly from its ext4
/// partition. Located by filesystem UUID when available (unambiguous even if
/// another attached drive carries the same label), falling back to label only
/// when the UUID is empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentEntry {
    pub name: String,
    pub label: String,
    /// Filesystem UUID captured at format time. Primary boot locator.
    pub fs_uuid: String,
    pub kernel: String,
    pub initrd: String,
    pub boot_params: String,
}

/// An ISO entry for the free space live session slot — loopback boot from FAT32.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveEntry {
    pub name: String,
    pub filename: String,
    pub kernel: String,
    pub initrd: String,
    pub boot_params: String,
    pub locate: String,
}

pub fn install_grub(efi_mnt: &str, dev: &str) -> Result<()> {
    let grub_cmd = if which("grub2-install") { "grub2-install" } else { "grub-install" };

    run_privileged(grub_cmd, &[
        "--target=x86_64-efi",
        &format!("--boot-directory={efi_mnt}/boot"),
        &format!("--efi-directory={efi_mnt}"),
        "--removable",
    ])?;

    run_privileged(grub_cmd, &[
        "--target=i386-pc",
        "--no-floppy",
        &format!("--boot-directory={efi_mnt}/boot"),
        dev,
    ])?;

    Ok(())
}

pub fn write_grub_config(
    efi_mnt: &str,
    persistent: &[PersistentEntry],
    live: &[LiveEntry],
) -> Result<()> {
    let grub_dir = if std::path::Path::new(&format!("{efi_mnt}/boot/grub2")).exists() {
        "grub2"
    } else {
        "grub"
    };

    let efi_cfg = format!(
        r#"insmod ext2
insmod fat
insmod part_gpt

if loadfont /{grub_dir}/fonts/unicode.pf2; then
  set gfxmode=auto
  if [ ${{grub_platform}} == "efi" ]; then
    insmod efi_gop
    insmod efi_uga
  else
    insmod all_video
  fi
  insmod gfxterm
  terminal_output gfxterm
fi

set menu_color_normal=white/black
set menu_color_highlight=black/light-gray
set gfxpayload=keep
set timeout=10

source /boot/{grub_dir}/bootospro.cfg
"#
    );

    let main_cfg = build_main_cfg(persistent, live);

    let pid = std::process::id();
    let tmp_efi  = format!("/tmp/bootospro-efi-{pid}.cfg");
    let tmp_main = format!("/tmp/bootospro-main-{pid}.cfg");

    std::fs::write(&tmp_efi, &efi_cfg)
        .map_err(|e| BootOsProError::Other(format!("write tmp efi cfg: {e}")))?;
    std::fs::write(&tmp_main, &main_cfg)
        .map_err(|e| BootOsProError::Other(format!("write tmp main cfg: {e}")))?;

    let grub_cfg_dir = format!("{efi_mnt}/boot/{grub_dir}");
    let efi_dest  = format!("{grub_cfg_dir}/grub.cfg");
    let main_dest = format!("{grub_cfg_dir}/bootospro.cfg");

    let result = run_privileged("cp", &[&tmp_efi, &efi_dest])
        .and_then(|_| run_privileged("cp", &[&tmp_main, &main_dest]));

    let _ = std::fs::remove_file(&tmp_efi);
    let _ = std::fs::remove_file(&tmp_main);

    result?;
    Ok(())
}

pub fn update_grub_config(
    efi_mnt: &str,
    persistent: &[PersistentEntry],
    live: &[LiveEntry],
) -> Result<()> {
    let grub_dir = if std::path::Path::new(&format!("{efi_mnt}/boot/grub2")).exists() {
        "grub2"
    } else {
        "grub"
    };

    let main_cfg = build_main_cfg(persistent, live);

    let pid = std::process::id();
    let tmp = format!("/tmp/bootospro-main-{pid}.cfg");
    std::fs::write(&tmp, &main_cfg)
        .map_err(|e| BootOsProError::Other(format!("write tmp cfg: {e}")))?;

    let main_dest = format!("{efi_mnt}/boot/{grub_dir}/bootospro.cfg");
    let result = run_privileged("cp", &[&tmp, &main_dest]);
    let _ = std::fs::remove_file(&tmp);
    result?;
    Ok(())
}

/// Build the menu body: every persistent OS first, then every live ISO.
/// This is a pure function of the entries passed in — the manifest is the
/// source those entries are derived from, so GRUB is always a faithful
/// projection of the manifest.
fn build_main_cfg(persistent: &[PersistentEntry], live: &[LiveEntry]) -> String {
    let mut cfg = String::new();
    for entry in persistent {
        cfg.push_str(&build_persistent_entry(entry));
    }
    for entry in live {
        cfg.push_str(&build_live_entry(entry));
    }
    cfg
}

// ── Menuentry builders ─────────────────────────────────────────────────────

fn build_persistent_entry(entry: &PersistentEntry) -> String {
    // Strip any casper/live boot directives — not needed for direct ext4 root mount.
    // The partition IS the root filesystem. Just mount it rw and boot.
    let params = strip_live_params(&entry.boot_params);

    let initrds: String = entry.initrd
        .split_whitespace()
        .map(|i| format!(" {i}"))
        .collect();

    // Locate the root by filesystem UUID when we have one — labels are not
    // unique across drives (a second stick or internal disk with the same
    // label sends GRUB and the kernel to the wrong filesystem). Label is the
    // fallback for records that predate UUID capture.
    let (search_line, root_param) = if entry.fs_uuid.is_empty() {
        (
            format!("search --label --no-floppy --set=root {}", entry.label),
            format!("root=LABEL={}", entry.label),
        )
    } else {
        (
            format!("search --fs-uuid --no-floppy --set=root {}", entry.fs_uuid),
            format!("root=UUID={}", entry.fs_uuid),
        )
    };

    format!(
        "menuentry '{name}' {{\n\
         \tinsmod ext2\n\
         \t{search_line}\n\
         \tlinux {kernel} {root_param} rw {params}\n\
         \tinitrd{initrds}\n\
         }}\n\n",
        name   = sanitize_menu_title(&entry.name),
        kernel = entry.kernel,
    )
}

/// Menu titles are user-typed display names interpolated into grub.cfg inside
/// single quotes. A quote, backslash, brace or newline in the name corrupts
/// the config. Whitelist the characters a display name legitimately needs.
fn sanitize_menu_title(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, ' ' | '.' | '-' | '_' | '+' | '(' | ')' | ':' | ',')
        })
        .take(64)
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() { "Unnamed OS".into() } else { trimmed }
}

/// Remove directives that only make sense for live ISO loopback boots.
/// For direct ext4 partition boots these cause the initrd to fail looking
/// for a squashfs that does not exist.
fn strip_live_params(params: &str) -> String {
    params
        .split_whitespace()
        .filter(|p| {
            !p.starts_with("boot=")
            && !p.starts_with("iso-scan")
            && !p.starts_with("findiso")
            && !p.starts_with("rd.live")
            && !p.starts_with("archisobasedir")
            && !p.starts_with("archisolabel")
            && *p != "persistent"
            && *p != "components"
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_live_entry(entry: &LiveEntry) -> String {
    let filename = &entry.filename;
    let initrds: String = entry.initrd
        .split_whitespace()
        .map(|i| format!(" (iso){i}"))
        .collect();

    // The live ISO's initrd must be told where to find the ISO file on the
    // FAT32 partition so it can loop-mount the squashfs. The method differs by
    // family (detected at write time, carried in `locate`):
    //   iso-scan → iso-scan/filename=/file.iso   (casper: Ubuntu/Mint)
    //   findiso  → findiso=/file.iso             (live-boot: Debian/Sparky)
    //   none     → family self-locates via a label param already in boot_params
    //              (Fedora root=live:CDLABEL=, Arch archisolabel=)
    let locate = match entry.locate.as_str() {
        "iso-scan" => format!(" iso-scan/filename=/{filename}"),
        "findiso"  => format!(" findiso=/{filename}"),
        _ => String::new(),
    };

    format!(
        "menuentry 'Live: {name}' {{\n\
         \tinsmod loopback\n\
         \tinsmod iso9660\n\
         \tinsmod fat\n\
         \tsearch --label --no-floppy --set=root FREESPACE\n\
         \tset isofile=\"/{filename}\"\n\
         \tloopback iso $isofile\n\
         \tlinux (iso){kernel} {params}{locate}\n\
         \tinitrd{initrds}\n\
         }}\n\n",
        name   = sanitize_menu_title(&entry.name),
        kernel = entry.kernel,
        params = entry.boot_params.trim(),
    )
}

fn which(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
