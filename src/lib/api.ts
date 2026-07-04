import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  BlockDevice,
  IsoWriteSpec,
  WriteProgressEvent,
  DriveReport,
} from './types';

// ── Device ──────────────────────────────────────────────────────────────────

export async function listDevices(): Promise<BlockDevice[]> {
  return invoke<BlockDevice[]>('list_devices');
}

// ── Full first write ─────────────────────────────────────────────────────────

export async function runFullWrite(
  device: string,
  driveLabel: string,
  isos: IsoWriteSpec[],
  free_space_gb: number,
): Promise<void> {
  const persistent = isos.filter(i => i.iso_type === 'persistent');
  const liveIsos = isos.filter(i => i.iso_type === 'live');
  return invoke('run_full_write', {
    args: {
      device,
      drive_label: driveLabel,
      isos: persistent.map(i => ({
        name: i.name,
        path: i.path,
        label: i.label,
        size_gb: i.size_gb,
        boot_params: i.boot_params,
        username: i.username,
        password: i.password,
      })),
      free_space_gb,
      // Live boot params are detected from the ISO structure at write time —
      // the backend does not accept them here.
      live_isos: liveIsos.map(i => ({
        name: i.name,
        path: i.path,
      })),
    },
  });
}

// ── Add persistent ISO to existing drive ────────────────────────────────────

export async function addPersistentIso(
  device: string,
  iso: IsoWriteSpec,
): Promise<void> {
  return invoke('add_persistent_iso', {
    args: {
      device,
      iso: {
        name: iso.name,
        path: iso.path,
        label: iso.label,
        size_gb: iso.size_gb,
        boot_params: iso.boot_params,
        username: iso.username,
        password: iso.password,
      },
    },
  });
}

// ── Add live ISO to existing drive (additive, no reformat) ──────────────────

export async function addLiveIso(
  device: string,
  iso: IsoWriteSpec,
): Promise<void> {
  return invoke('add_live_iso', {
    args: {
      device,
      iso: { name: iso.name, path: iso.path },
    },
  });
}

// ── Scalpel operations ───────────────────────────────────────────────────────

export async function formatPersistentSlot(
  device: string,
  partition: string,
  label: string,
): Promise<void> {
  return invoke('format_persistent_slot', { args: { device, partition, label } });
}

export async function formatFreeSpaceSlot(device: string): Promise<void> {
  return invoke('format_free_space_slot', { args: { device } });
}

// ── Manifest ─────────────────────────────────────────────────────────────────

export async function readDriveManifest(device: string): Promise<DriveReport> {
  return invoke<DriveReport>('read_drive_manifest', { device });
}

export async function reconcileManifest(
  device: string,
  driveLabel: string,
): Promise<void> {
  return invoke('reconcile_manifest', { device, driveLabel });
}

// ── Cancellation ─────────────────────────────────────────────────────────────

export async function cancelOperation(): Promise<void> {
  return invoke('cancel_operation');
}

// ── Event listeners ──────────────────────────────────────────────────────────

export function onWriteProgress(
  cb: (e: WriteProgressEvent) => void,
): Promise<UnlistenFn> {
  return listen<WriteProgressEvent>('write_progress', (event) => cb(event.payload));
}

// ── Disk Manager ─────────────────────────────────────────────────────────────

export async function getPartitionLayout(device: string): Promise<BlockDevice> {
  return invoke('get_partition_layout', { device });
}

export async function formatPartition(
  partition: string,
  fstype: string,
  label: string,
): Promise<void> {
  return invoke('format_partition', { args: { partition, fstype, label } });
}

export async function deletePartition(
  device: string,
  partition: string,
): Promise<void> {
  return invoke('delete_partition', { args: { device, partition } });
}

export async function wipeDevice(device: string, label: string, fstype: string): Promise<void> {
  return invoke('wipe_device', { args: { device, label, fstype } });
}

export async function unmountPartition(partition: string): Promise<void> {
  return invoke('unmount_partition', { partition });
}

// ── File utilities ───────────────────────────────────────────────────────────

export async function getFileSize(path: string): Promise<number> {
  return invoke<number>('get_file_size', { path });
}

// ── ISO architecture detection ────────────────────────────────────────────────

export async function getIsoArch(path: string): Promise<string> {
  return invoke<string>('get_iso_arch', { path });
}
