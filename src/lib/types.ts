export interface BlockDevice {
  path: string;
  model: string;
  size_bytes: number;
  size_human: string;
  transport: string;
  removable: boolean;
  children: Partition[];
}

export interface Partition {
  path: string;
  size_bytes: number;
  fstype: string;
  label: string;
  mountpoint: string | null;
  part_type: string;
}

export interface IsoWriteSpec {
  name: string;
  path: string;
  label: string;
  size_gb: number;
  boot_params: string;
  file_size_bytes: number;
  iso_type: 'persistent' | 'live';
  username: string;
  password: string;
}

export interface WriteProgressEvent {
  stage: 'partition' | 'format' | 'grub' | 'extract' | 'copy' | 'config' | 'sync';
  pct: number;
  msg: string;
}

export type Step = 'configure' | 'summary' | 'write';

export interface AppState {
  step: Step;
  device: BlockDevice | null;
  isos: IsoWriteSpec[];
  free_space_gb: number;
  label: string;
}

// ── Manifest (mirrors src-tauri/src/manifest.rs) ────────────────────────────

export type SlotState = 'filled' | 'empty';

export interface PersistentRecord {
  label: string;
  os_name: string | null;
  kernel: string;
  initrd: string;
  boot_params: string;
  size_bytes: number;
  fs_uuid: string;
  state: SlotState;
}

export interface LiveRecord {
  filename: string;
  os_name: string;
  kernel: string;
  initrd: string;
  boot_params: string;
  locate: string;
  size_bytes: number;
}

export interface FreeSpaceRecord {
  label: string;
  size_bytes: number;
  fs_uuid: string;
}

export interface InstallerPackage {
  platform: string;
  filename: string;
  version: string;
  sha256: string;
}

export interface InstallerRecord {
  present: boolean;
  packages: InstallerPackage[];
}

export interface Manifest {
  schema_version: number;
  app_version: string;
  updated: string;
  drive_label: string;
  persistent: PersistentRecord[];
  live: LiveRecord[];
  free_space: FreeSpaceRecord;
  installer: InstallerRecord;
  body_hash: string;
}

export interface DriveReport {
  manifest: Manifest | null;
  drift: boolean;
}
