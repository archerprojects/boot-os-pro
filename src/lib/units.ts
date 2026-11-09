// Boot OS Pro — units.ts — build 1.0.2 — Monday 9 November 2026
//
// One unit everywhere: the backend allocates partitions in GiB
// (size_gb * 1024^3), so every size the UI computes or shows is binary too.
// A size the user picks is the size that gets created.

export const MIB = 1024 ** 2;
export const GIB = 1024 ** 3;

// BOOTOSPRO installer partition on new drives, matching disk.rs INSTALLER_MIB.
export const INSTALLER_MIB = 256;

// Fixed layout overhead on a new drive, matching disk.rs partition_drive_full:
// 1 MiB alignment + 1 MiB BIOS Boot + 512 MiB ESP + BOOTOSPRO
// + 1 MiB tail reserve.
export const FIXED_OVERHEAD_GIB = (1 + 1 + 512 + INSTALLER_MIB + 1) / 1024;

// Largest file FAT32 can hold (drives made before 1.0.19 have a FAT32
// free space partition).
export const FAT32_MAX_FILE = 4 * GIB - 1;

// Unpacked-to-compressed ratio used when the ISO does not record its unpacked
// size, matching squash.rs UNPACK_RATIO_ESTIMATE.
export const UNPACK_RATIO_ESTIMATE = 3.3;

// Smallest reserved slot offered for a future persistent OS.
export const MIN_SLOT_GIB = 8;

// Carve headroom: the backend keeps at least 1 MiB of free space beyond a
// carved partition, so the ceiling sits just below the free space size.
export const CARVE_HEADROOM_GIB = 0.01;

export function formatGib(gib: number): string {
  return `${gib % 1 === 0 ? gib : gib.toFixed(1)} GiB`;
}

export function formatBytes(bytes: number): string {
  if (bytes >= GIB) return `${(bytes / GIB).toFixed(1)} GiB`;
  return `${(bytes / MIB).toFixed(0)} MiB`;
}
