// Boot OS Pro — ImagesStep.tsx — build 1.0.2 — Monday 9 November 2026

import { useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { homeDir } from '@tauri-apps/api/path';
import { listDevices, getFileSize, getPartitionLayout, getIsoInfo } from '../lib/api';
import type { IsoWriteSpec, BlockDevice, Partition, DriveReport } from '../lib/types';
import {
  GIB, MIB, FIXED_OVERHEAD_GIB, CARVE_HEADROOM_GIB, FAT32_MAX_FILE,
  UNPACK_RATIO_ESTIMATE, MIN_SLOT_GIB,
} from '../lib/units';

interface Props {
  device: BlockDevice | null;
  isos: IsoWriteSpec[];
  freeSpaceGb: number;
  report: DriveReport | null;
  isExisting: boolean;
  onDeviceSelect: (d: BlockDevice) => void;
  onChange: (isos: IsoWriteSpec[]) => void;
  onReconcile: () => Promise<void>;
  onNext: () => void;
}

const BOOT_PARAMS: Record<string, string> = {
  ubuntu: 'boot=casper quiet splash',
  mint:   'boot=casper quiet splash',
  fedora: 'rd.live.image quiet',
  debian: 'boot=live components quiet splash',
  arch:   'quiet',
  custom: 'quiet splash',
};

function detectDistro(filename: string): string {
  const f = filename.toLowerCase();
  if (f.includes('mint'))   return 'mint';
  if (f.includes('ubuntu')) return 'ubuntu';
  if (f.includes('fedora')) return 'fedora';
  if (f.includes('debian')) return 'debian';
  if (f.includes('arch'))   return 'arch';
  return 'custom';
}

function makeLabel(name: string, existing: IsoWriteSpec[]): string {
  const base = name.toUpperCase().replace(/[^A-Z0-9]/g, '').slice(0, 10);
  let label = base;
  let n = 1;
  while (existing.some(i => i.label === label)) {
    label = base.slice(0, 9) + String(n++);
  }
  return label || 'ISO' + String(existing.length + 1);
}

function formatBytes(bytes: number): string {
  if (bytes >= GIB) return `${(bytes / GIB).toFixed(1)} GiB`;
  return `${(bytes / MIB).toFixed(0)} MiB`;
}

function formatGb(gb: number): string {
  return `${gb % 1 === 0 ? gb : gb.toFixed(1)} GiB`;
}

// Same rule as the backend (jobs.rs valid_username): accepted by useradd on
// every supported family.
const USERNAME_RE = /^[a-z][a-z0-9_-]{0,31}$/;

// Persistent sizing: the default is twice the real unpacked size (room to
// install and update); the minimum is the unpacked size plus 10 %.
function unpackedGib(e: IsoWriteSpec): number {
  return e.unpacked_bytes > 0
    ? e.unpacked_bytes / GIB
    : (e.file_size_bytes * UNPACK_RATIO_ESTIMATE) / GIB;
}
function defaultSizeGib(e: IsoWriteSpec): number {
  return Math.max(1, Math.ceil(unpackedGib(e) * 2));
}
function minSizeGib(e: IsoWriteSpec): number {
  return e.iso_type === 'reserved' ? MIN_SLOT_GIB : Math.max(1, Math.ceil(unpackedGib(e) * 1.1));
}

/** An empty reserved slot on an existing drive. */
interface EmptySlot { label: string; sizeGb: number; }

// Classify partitions from real drive layout
interface DriveLayout {
  efiGb: number;
  biosBootGb: number;
  existingPersistentGb: number;
  existingPartitions: Partition[];
  totalGb: number;
  /** Filesystem of the FREESPACE partition ('vfat', 'exfat', or '' if none). */
  freeFstype: string;
}

function parseDriveLayout(device: BlockDevice): DriveLayout {
  const totalGb = device.size_bytes / GIB;
  let efiGb = 0;
  let biosBootGb = 0;
  let existingPersistentGb = 0;
  let freeFstype = '';
  const existingPartitions: Partition[] = [];

  for (const p of device.children) {
    const labelUp = p.label.toUpperCase();
    const fsUp = p.fstype.toUpperCase();
    const gb = p.size_bytes / GIB;

    // BIOS Boot — tiny unformatted, type GUID 21686148
    if (p.part_type.startsWith('21686148') || labelUp === 'BIOS BOOT' || labelUp === 'BIOS_BOOT') {
      biosBootGb += gb;
    }
    // EFI
    else if (fsUp.includes('FAT') && (labelUp === 'ESP' || labelUp === 'EFI' || gb < 1)) {
      efiGb += gb;
    }
    // Free space partition
    else if (labelUp === 'FREESPACE' || labelUp === 'FREE SPACE') {
      freeFstype = p.fstype.toLowerCase();
      existingPartitions.push(p);
    }
    // Persistent ext4 partition
    else if (p.fstype === 'ext4') {
      existingPersistentGb += gb;
      existingPartitions.push(p);
    }
    // Any other FAT partition that isn't EFI
    else if (fsUp.includes('FAT') || fsUp === 'EXFAT') {
      existingPartitions.push(p);
    }
    else if (p.fstype) {
      existingPartitions.push(p);
    }
  }

  // If no EFI found but drive has partitions, use 0.5 GiB as fallback
  if (efiGb === 0 && device.children.length > 0) efiGb = 0.5;

  return { efiGb, biosBootGb, existingPersistentGb, existingPartitions, totalGb, freeFstype };
}

// ── Configure ISO page ─────────────────────────────────────────────────────

interface ConfigureProps {
  entry: IsoWriteSpec;
  isNew: boolean;
  isos: IsoWriteSpec[];
  editIdx: number | null;
  driveGb: number;
  existingPersistentGb: number;
  maxPersistentOverrideGb?: number;
  /** Existing drive: empty slots not already taken by another queued ISO. */
  emptySlots: EmptySlot[];
  /** Existing drive whose free space partition is FAT32 (4 GiB file limit). */
  freeIsFat32: boolean;
  onSave: (entry: IsoWriteSpec) => void;
  onCancel: () => void;
}

function ConfigureIso({
  entry, isNew, isos, editIdx, driveGb,
  existingPersistentGb, maxPersistentOverrideGb, emptySlots, freeIsFat32, onSave, onCancel,
}: ConfigureProps) {
  const [e, setE] = useState<IsoWriteSpec>({ ...entry });
  const [typeChosen, setTypeChosen] = useState(!isNew || entry.iso_type === 'reserved');
  const [sizeText, setSizeText] = useState(String(entry.size_gb));

  const isReserved   = e.iso_type === 'reserved';
  const isPersistent = e.iso_type === 'persistent';
  const isLive       = e.iso_type === 'live';
  const usesPartition = isPersistent || isReserved;
  const slot = isPersistent ? emptySlots.find(sl => sl.label === e.target_slot) : undefined;

  // Live ISOs all share the free space partition. We track how many are already
  // queued and the total live bytes so we can show free-space awareness, but we
  // do NOT restrict to one — multiple live ISOs coexist, GRUB lists each.
  const sessionLiveCount = isos.filter((i, idx) => i.iso_type === 'live' && idx !== editIdx).length;
  const sessionLiveBytes = isos
    .filter((i, idx) => i.iso_type === 'live' && idx !== editIdx)
    .reduce((s, i) => s + i.file_size_bytes, 0);

  // Space already taken by other queued partitions (slot installs take none).
  const sessionPartitionGb = isos
    .filter((_, idx) => idx !== editIdx)
    .filter(i => i.iso_type !== 'live' && !i.target_slot)
    .reduce((s, i) => s + i.size_gb, 0);

  const totalAllocated = FIXED_OVERHEAD_GIB + existingPersistentGb + sessionPartitionGb;
  // On an existing drive, a new persistent partition is carved from the real
  // FREESPACE partition, so the ceiling is that free space minus what this
  // session already queued — not the whole-drive arithmetic.
  const availableGb = maxPersistentOverrideGb !== undefined
    ? Math.max(0, maxPersistentOverrideGb - sessionPartitionGb - CARVE_HEADROOM_GIB)
    : Math.max(0, driveGb - totalAllocated);
  const fitsGb = Math.floor(availableGb);

  const minGb = minSizeGib(e);
  const sizeGb = Number(sizeText);

  // Why the size cannot be used — shown beside the field and on Add.
  let sizeError: string | null = null;
  if (usesPartition && slot) {
    if (slot.sizeGb < minGb) {
      sizeError = `${slot.label} is ${formatGb(slot.sizeGb)}; this OS needs at least ${minGb} GiB.`;
    }
  } else if (usesPartition) {
    if (!/^\d+$/.test(sizeText)) sizeError = 'Enter a whole number of GiB.';
    else if (sizeGb < minGb)     sizeError = `At least ${minGb} GiB is needed.`;
    else if (sizeGb > fitsGb)    sizeError = `Only ${fitsGb} GiB is free.`;
  }
  const tooBigForFat32 = isLive && freeIsFat32 && e.file_size_bytes > FAT32_MAX_FILE;

  const chooseType = (t: 'persistent' | 'live') => {
    setE({ ...e, iso_type: t });
    setTypeChosen(true);
  };

  const save = () => onSave({
    ...e,
    size_gb: slot ? slot.sizeGb : usesPartition ? sizeGb : e.size_gb,
  });

  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>{isReserved ? (isNew ? 'Reserve empty slot' : 'Edit empty slot') : (isNew ? 'Add ISO' : 'Edit ISO')}</h1>
        {isReserved && (
          <p>An empty ext4 partition kept for a persistent OS you add later.
            Nothing is installed in it now.</p>
        )}
      </div>

      <div className="card form-card">
        {!isReserved && (
          <label className="field">
            <span>Display name</span>
            <input
              value={e.name}
              onChange={ev => setE({ ...e, name: ev.target.value })}
              placeholder="e.g. Linux Mint 22"
            />
          </label>
        )}

        {!isReserved && (
          <div className="field">
            <span>Type</span>
            <div className="field-hint" style={{ marginBottom: '0.4rem' }}>
              Select the Drive Type below to Continue
            </div>
            <div className="type-grid">
              <button
                type="button"
                className={`type-card ${typeChosen && isLive ? 'type-active' : ''} ${!typeChosen ? 'type-unset' : ''}`}
                onClick={() => chooseType('live')}
              >
                <div className="type-radio">
                  <div className={`radio-dot ${typeChosen && isLive ? 'on' : ''}`} />
                </div>
                <div className="type-body">
                  <div className="type-title">Live Session</div>
                  <div className="type-sub">Free Space Partition</div>
                  <div className="type-sub">No Persistence</div>
                </div>
              </button>

              <button
                type="button"
                className={`type-card ${typeChosen && isPersistent ? 'type-active' : ''} ${!typeChosen ? 'type-unset' : ''}`}
                onClick={() => chooseType('persistent')}
              >
                <div className="type-radio">
                  <div className={`radio-dot ${typeChosen && isPersistent ? 'on' : ''}`} />
                </div>
                <div className="type-body">
                  <div className="type-title">Persistent OS</div>
                  <div className="type-sub">EXT4 Partition</div>
                  <div className="type-sub">Apps and Data Survive Reboot</div>
                </div>
              </button>
            </div>
            {typeChosen && isLive && (
              <div className="info-panel">
                Live ISOs share the free space partition — you can add several and
                pick which to boot from the GRUB menu. They run without persistence
                (changes are lost on reboot).
                {sessionLiveCount > 0 &&
                  ` ${sessionLiveCount} live ISO${sessionLiveCount > 1 ? 's' : ''} already queued (${formatBytes(sessionLiveBytes)}).`}
              </div>
            )}
            {typeChosen && tooBigForFat32 && (
              <div className="warn-box" style={{ marginTop: '0.5rem' }}>
                This ISO is {formatBytes(e.file_size_bytes)}. The free space partition on
                this drive is FAT32, which cannot hold a file of 4 GiB or more. Use Clear
                Live in Disk Manager to re-format it as exFAT, then add the ISO again.
              </div>
            )}
          </div>
        )}

        {typeChosen && isPersistent && emptySlots.length > 0 && (
          <div className="field">
            <span>Install into</span>
            <select
              className="text-input"
              value={e.target_slot}
              onChange={ev => setE({ ...e, target_slot: ev.target.value })}
            >
              <option value="">New partition from free space</option>
              {emptySlots.map(sl => (
                <option key={sl.label} value={sl.label}>
                  Empty slot {sl.label} ({formatGb(sl.sizeGb)})
                </option>
              ))}
            </select>
            {slot && (
              <div className="field-hint">
                The OS is installed into {slot.label}. Free space and the live ISOs on
                it are kept.
              </div>
            )}
          </div>
        )}

        {typeChosen && usesPartition && !slot && (
          <label className="field">
            <span>Partition size (GiB)</span>
            <input
              inputMode="numeric"
              value={sizeText}
              onChange={ev => setSizeText(ev.target.value.replace(/[^0-9]/g, ''))}
              style={{ width: '8rem' }}
            />
            <div className="field-hint">
              Free: {fitsGb} GiB. Minimum: {minGb} GiB
              {isPersistent && (e.unpacked_bytes > 0
                ? ` (this OS unpacks to ${formatBytes(e.unpacked_bytes)})`
                : ' (estimated from the ISO size)')}.
              {isPersistent && ` Suggested: ${defaultSizeGib(e)} GiB.`}
            </div>
            {sizeError && <div className="field-error">{sizeError}</div>}
          </label>
        )}
        {slot && sizeError && <div className="field-error">{sizeError}</div>}

        {!isReserved && (
          <div className="field">
            <span>ISO file</span>
            <input value={e.path} readOnly className="input-readonly" />
            {e.file_size_bytes > 0 && (
              <div className="field-hint">{formatBytes(e.file_size_bytes)}</div>
            )}
          </div>
        )}

        {typeChosen && usesPartition && (
          <label className="field">
            <span>Partition label</span>
            <input
              value={e.label}
              maxLength={11}
              onChange={ev => setE({ ...e, label: ev.target.value.toUpperCase().replace(/[^A-Z0-9]/g, '') })}
              placeholder={isReserved ? 'SLOT1' : 'MINT223'}
            />
          </label>
        )}

        {typeChosen && isPersistent && (
          <>
            <label className="field">
              <span>Boot parameters</span>
              <input
                value={e.boot_params}
                onChange={ev => setE({ ...e, boot_params: ev.target.value })}
                placeholder="quiet splash"
              />
            </label>
            <div className="info-panel">
              This persistent OS gets its own login. Set a username and password —
              they are required to log in after boot. A portable drive you carry
              should not be left without a password.
            </div>
            <label className="field">
              <span>Username</span>
              <input
                value={e.username}
                onChange={ev => setE({ ...e, username: ev.target.value.toLowerCase().replace(/[^a-z0-9_-]/g, '').slice(0, 32) })}
                placeholder="e.g. mint"
                autoComplete="off"
              />
              <div className="field-hint">
                Lowercase letters, digits, _ or -, starting with a letter.
              </div>
            </label>
            <label className="field">
              <span>Password</span>
              <input
                type="password"
                value={e.password}
                onChange={ev => setE({ ...e, password: ev.target.value })}
                placeholder="Enter a password"
                autoComplete="new-password"
              />
              <div className="field-hint">Used to log in to this OS after booting.</div>
            </label>
          </>
        )}
      </div>

      <div className="action-bar">
        <button className="btn" onClick={onCancel}>Cancel</button>
        <button
          className="btn btn-primary"
          disabled={
            !typeChosen ||
            (!isReserved && !e.name) ||
            (usesPartition && (!!sizeError || !e.label)) ||
            (isPersistent && (!USERNAME_RE.test(e.username) || !e.password)) ||
            tooBigForFat32
          }
          title={sizeError ?? (tooBigForFat32 ? 'Too large for this drive’s FAT32 free space.' : undefined)}
          onClick={save}
        >
          {isNew ? '+ Add' : '✓ Save'}
        </button>
      </div>
    </div>
  );
}

// ── Combined Device + Images page ─────────────────────────────────────────

export function ImagesStep({ device, isos, freeSpaceGb, report, isExisting, onDeviceSelect, onChange, onReconcile, onNext }: Props) {
  const [devices, setDevices]         = useState<BlockDevice[]>([]);
  const [devLoading, setDevLoading]   = useState(true);
  const [layoutLoading, setLayoutLoading] = useState(false);
  const [realLayout, setRealLayout]   = useState<DriveLayout | null>(null);
  const [configuring, setConfiguring] = useState<IsoWriteSpec | null>(null);
  const [editIdx, setEditIdx]         = useState<number | null>(null);
  const [isoLoading, setIsoLoading]   = useState(false);
  const [archWarning, setArchWarning] = useState<string | null>(null);
  const [reconciling, setReconciling]   = useState(false);
  const [reconcileErr, setReconcileErr] = useState<string | null>(null);

  const handleReconcile = async () => {
    setReconciling(true);
    setReconcileErr(null);
    try {
      await onReconcile();
    } catch (e) {
      setReconcileErr(String(e));
    }
    setReconciling(false);
  };

  const refresh = async () => {
    setDevLoading(true);
    try { setDevices(await listDevices()); } catch { setDevices([]); }
    setDevLoading(false);
  };

  useEffect(() => { refresh(); }, []);

  // When a device is selected, read its real partition layout immediately
  const selectDevice = async (dev: BlockDevice) => {
    onDeviceSelect(dev);
    setRealLayout(null);
    setLayoutLoading(true);
    try {
      const layout = await getPartitionLayout(dev.path);
      setRealLayout(parseDriveLayout(layout));
    } catch {
      // Drive may be blank — use device size only
      setRealLayout({
        efiGb: 0,
        biosBootGb: 0,
        existingPersistentGb: 0,
        existingPartitions: [],
        totalGb: dev.size_bytes / GIB,
        freeFstype: '',
      });
    }
    setLayoutLoading(false);
  };

  const driveGb = realLayout?.totalGb ?? (device?.size_bytes ?? 0) / GIB;

  // On an existing Boot OS Pro drive, the real free space is the FREESPACE
  // partition's actual size (from the manifest) minus whatever live ISOs the
  // user has queued this session. New persistent adds carve from this, so the
  // sliders must be bounded by it — never by whole-drive arithmetic.
  const manifestFreeGb = report?.manifest
    ? report.manifest.free_space.size_bytes / GIB
    : 0;
  const queuedLiveGb = isos
    .filter(i => i.iso_type === 'live')
    .reduce((s, i) => s + i.file_size_bytes / GIB, 0);
  const queuedNewPersistentGb = isos
    .filter(i => i.iso_type !== 'live' && !i.target_slot)
    .reduce((s, i) => s + i.size_gb, 0);
  const realFreeGb = isExisting
    ? Math.max(0, manifestFreeGb - queuedLiveGb - queuedNewPersistentGb)
    : freeSpaceGb;

  // Space graphic values — always read from real layout + session additions.
  // A drive that is not a Boot OS Pro drive is erased by the write, so its old
  // partitions take no space and are not drawn.
  const realEfiGb        = realLayout?.efiGb ?? 0;
  const realPersistentGb = isExisting ? (realLayout?.existingPersistentGb ?? 0) : 0;
  const freeIsFat32 = isExisting && realLayout?.freeFstype === 'vfat';

  // Empty slots on an existing drive that no queued ISO is installing into.
  const emptySlotsFor = (idx: number | null): EmptySlot[] =>
    (isExisting ? report?.manifest?.persistent ?? [] : [])
      .filter(p => p.state === 'empty')
      .filter(p => !isos.some((i, j) => j !== idx && i.target_slot === p.label))
      .map(p => ({ label: p.label, sizeGb: Math.floor(p.size_bytes / GIB) }));
  const efiDisplayGb = realEfiGb > 0 ? realEfiGb : (device ? 0.5 : 0);

  const addIso = async () => {
    const home = await homeDir().catch(() => '/home');
    const sel = await open({
      multiple: false,
      defaultPath: `${home}/Downloads`,
      filters: [{ name: 'ISO Images', extensions: ['iso', 'img'] }],
    });
    if (!sel) return;
    const path = typeof sel === 'string' ? sel : sel[0];
    const filename = path.split('/').pop() ?? path;
    const distro = detectDistro(filename);

    setIsoLoading(true);
    let file_size_bytes = 0;
    let arch = 'unknown';
    let unpacked_bytes = 0;
    try { file_size_bytes = await getFileSize(path); } catch { /* non-fatal */ }
    try {
      const info = await getIsoInfo(path);
      arch = info.arch;
      unpacked_bytes = info.unpacked_bytes;
    } catch { /* non-fatal */ }
    setIsoLoading(false);

    if (arch === 'aarch64') {
      setArchWarning(
        'This ISO is built for ARM (aarch64). Boot OS Pro installs an x86_64 GRUB ' +
        'bootloader. This ISO will not boot on x86_64 hardware, and the drive will ' +
        'not be recognised as bootable on ARM hardware either (ARM EFI firmware ' +
        'requires an ARM64 bootloader which this app does not yet install). ' +
        'Proceed only if you know what you are doing.'
      );
    } else {
      setArchWarning(null);
    }

    const entry: IsoWriteSpec = {
      name: filename.replace(/\.[^.]+$/, '').replace(/[-_]/g, ' '),
      path,
      label: makeLabel(filename.replace(/\.[^.]+$/, ''), isos),
      size_gb: 0,
      boot_params: BOOT_PARAMS[distro] ?? BOOT_PARAMS.custom,
      file_size_bytes,
      unpacked_bytes,
      iso_type: 'persistent',
      target_slot: '',
      username: '',
      password: '',
    };
    entry.size_gb = defaultSizeGib(entry);
    setConfiguring(entry);
    setEditIdx(null);
  };

  const reserveSlot = () => {
    setArchWarning(null);
    setConfiguring({
      name: 'Empty slot',
      path: '',
      label: makeLabel('SLOT', isos),
      size_gb: 20,
      boot_params: '',
      file_size_bytes: 0,
      unpacked_bytes: 0,
      iso_type: 'reserved',
      target_slot: '',
      username: '',
      password: '',
    });
    setEditIdx(null);
  };

  const saveConfig = (entry: IsoWriteSpec) => {
    if (editIdx === null) {
      onChange([...isos, entry]);
    } else {
      const updated = [...isos];
      updated[editIdx] = entry;
      onChange(updated);
    }
    setConfiguring(null);
    setEditIdx(null);
  };

  const removeIso = (i: number) => onChange(isos.filter((_, idx) => idx !== i));
  const startEdit = (iso: IsoWriteSpec, i: number) => {
    setConfiguring({ ...iso });
    setEditIdx(i);
  };

  if (configuring) {
    return (
      <>
        {archWarning && (
          <div className="warn-box" style={{ marginBottom: '0.75rem' }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" style={{ flexShrink: 0, marginTop: 2 }}>
              <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
              <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
            </svg>
            <span><strong>Architecture mismatch — </strong>{archWarning}</span>
          </div>
        )}
        <ConfigureIso
          entry={configuring}
          isNew={editIdx === null}
          isos={isos}
          editIdx={editIdx}
          driveGb={driveGb}
          existingPersistentGb={realPersistentGb}
          maxPersistentOverrideGb={isExisting ? manifestFreeGb - queuedLiveGb : undefined}
          emptySlots={emptySlotsFor(editIdx)}
          freeIsFat32={freeIsFat32}
          onSave={saveConfig}
          onCancel={() => { setConfiguring(null); setEditIdx(null); setArchWarning(null); }}
        />
      </>
    );
  }

  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>{device ? 'Configure drive' : 'Select a drive'}</h1>
        <p>{device
          ? (isExisting
              ? 'This is a Boot OS Pro drive. Add ISOs to the free space — existing contents are kept.'
              : 'Add your ISOs. This drive will be created from scratch.')
          : 'Select a USB connected drive to begin.'}</p>
      </div>

      {/* Device selector */}
      <div className="card">
        <div className="section-label">USB Device</div>
        {devLoading && (
          <div style={{ display: 'flex', gap: '8px', alignItems: 'center', padding: '0.5rem 0', color: 'var(--text-secondary)', fontSize: '13px' }}>
            <div className="spinner" style={{ width: '14px', height: '14px' }} />
            Scanning…
          </div>
        )}
        {!devLoading && devices.length === 0 && (
          <div style={{ display: 'flex', gap: '8px', alignItems: 'center', padding: '0.5rem 0', color: 'var(--text-secondary)', fontSize: '13px' }}>
            No USB connected drives found.
            <button className="btn" onClick={refresh}>↻ Refresh</button>
          </div>
        )}
        {!devLoading && devices.map(dev => (
          <button
            key={dev.path}
            className={`device-row ${device?.path === dev.path ? 'selected' : ''}`}
            onClick={() => selectDevice(dev)}
          >
            <div className={`radio ${device?.path === dev.path ? 'checked' : ''}`} />
            <div className="device-icon">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                <rect x="7" y="2" width="10" height="20" rx="2"/><path d="M12 18h.01"/>
              </svg>
            </div>
            <div className="device-info">
              <div className="device-name">{dev.model}</div>
              <div className="device-meta">{dev.path} · {dev.size_human}</div>
            </div>
            <div className="device-size">{dev.size_human}</div>
          </button>
        ))}
        {!devLoading && devices.length > 0 && (
          <button className="btn-link" style={{ marginTop: '0.4rem', fontSize: '11px' }} onClick={refresh}>
            ↻ Refresh device list
          </button>
        )}
        {device && isExisting && report?.manifest && (
          <>
            <div className="info-panel" style={{ marginTop: '0.75rem' }}>
              Existing Boot OS Pro drive — contents below are kept. New ISOs are
              added to the free space; nothing here is erased. To remove or
              reformat an OS, use Disk Manager.
            </div>
            <div className="drive-contents">
              <div className="section-label" style={{ marginTop: '0.6rem' }}>On this drive</div>
              {report.manifest.persistent.map((p, i) => (
                <div key={`p-${i}`} className="content-row">
                  <span className="content-dot key-persistent" />
                  <span className="content-name">
                    {p.state === 'filled' ? (p.os_name || p.label) : `${p.label} — empty slot`}
                  </span>
                  <span className="content-meta">persistent · {formatGb(p.size_bytes / GIB)}</span>
                </div>
              ))}
              {report.manifest.live.map((l, i) => (
                <div key={`l-${i}`} className="content-row">
                  <span className="content-dot key-free" />
                  <span className="content-name">{l.os_name}</span>
                  <span className="content-meta">live · {formatBytes(l.size_bytes)}</span>
                </div>
              ))}
              <div className="content-row">
                <span className="content-dot key-free" />
                <span className="content-name">Free space</span>
                <span className="content-meta">{formatGb(realFreeGb)} available</span>
              </div>
              {report.drift && (
                <div className="warn-box" style={{ marginTop: '0.5rem', display: 'block' }}>
                  <div style={{ marginBottom: '0.5rem' }}>
                    This drive was modified outside Boot OS Pro — the layout
                    shown may not match what is physically on the drive.
                    Rebuild the record from the disk to continue.
                  </div>
                  <button
                    className="btn"
                    disabled={reconciling}
                    onClick={handleReconcile}
                  >
                    {reconciling ? 'Rebuilding…' : 'Rebuild from disk'}
                  </button>
                  {reconcileErr && (
                    <div style={{ marginTop: '0.5rem', color: 'var(--error)', fontSize: '12px' }}>
                      Rebuild failed: {reconcileErr}
                    </div>
                  )}
                </div>
              )}
            </div>
          </>
        )}
        {device && !isExisting && (
          <div className="warn-box" style={{ marginTop: '0.75rem' }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
              <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
            </svg>
            This is not a Boot OS Pro drive. Continuing will erase everything on it
            and create a new multiboot drive from scratch.
          </div>
        )}
      </div>

      {/* ISO list */}
      <div className="card">
        <div className="section-label">ISOs</div>
        {isos.length === 0 && (
          <div style={{ display: 'flex', alignItems: 'center', gap: '8px', padding: '0.4rem 0', color: 'var(--text-tertiary)', fontSize: '13px' }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
              <circle cx="12" cy="12" r="10"/><path d="M8 12h8M12 8v8"/>
            </svg>
            No ISOs added yet.
          </div>
        )}
        {isos.map((iso, i) => (
          <div key={i} className="iso-card">
            <div className="iso-main">
              <div className="iso-name">{iso.name}</div>
              <div className="iso-path">
                {iso.iso_type === 'reserved'
                  ? 'Reserved for a persistent OS added later'
                  : iso.path.split('/').pop()}
                {iso.file_size_bytes > 0 && ` · ${formatBytes(iso.file_size_bytes)}`}
              </div>
              <div className="iso-badges">
                {iso.iso_type === 'persistent' && (
                  <>
                    <span className="badge badge-blue">persistent</span>
                    <span className="badge badge-blue-dim">{iso.size_gb} GiB</span>
                    <span className="badge badge-gray">{iso.label}</span>
                    {iso.target_slot && <span className="badge badge-gray">into {iso.target_slot}</span>}
                  </>
                )}
                {iso.iso_type === 'reserved' && (
                  <>
                    <span className="badge badge-gray">empty slot</span>
                    <span className="badge badge-blue-dim">{iso.size_gb} GiB</span>
                    <span className="badge badge-gray">{iso.label}</span>
                  </>
                )}
                {iso.iso_type === 'live' && <span className="badge badge-green">live session</span>}
              </div>
            </div>
            <div className="iso-actions">
              <button className="btn-icon" onClick={() => startEdit(iso, i)} title="Edit">✎</button>
              <button className="btn-icon danger" onClick={() => removeIso(i)} title="Remove">✕</button>
            </div>
          </div>
        ))}
        <button className="add-iso-btn" onClick={addIso} disabled={isoLoading || !device}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <circle cx="12" cy="12" r="10"/><path d="M12 8v8M8 12h8"/>
          </svg>
          {isoLoading ? 'Reading file…' : 'Add ISO'}
        </button>
        {device && !isExisting && (
          <button className="add-iso-btn" onClick={reserveSlot} disabled={isoLoading}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <rect x="3" y="6" width="18" height="12" rx="2"/>
            </svg>
            Reserve empty slot
          </button>
        )}
      </div>

      {/* Drive space graphic — reads from REAL partition layout */}
      {device && driveGb > 0 && (
        <div className="card">
          <div className="space-key">
            <span><span className="key-dot key-efi" /> EFI (permanent)</span>
            <span><span className="key-dot key-persistent" /> Persistent</span>
            <span><span className="key-dot key-free" /> Free Space</span>
          </div>

          {layoutLoading ? (
            <div style={{ display: 'flex', gap: '8px', alignItems: 'center', fontSize: '12px', color: 'var(--text-secondary)', marginTop: '0.5rem' }}>
              <div className="spinner" style={{ width: '12px', height: '12px' }} /> Reading drive layout…
            </div>
          ) : (
            <>
              <div className="space-bar" style={{ marginTop: '0.5rem' }}>
                {efiDisplayGb > 0 && (
                  <div className="space-seg seg-efi-red"
                    style={{ width: `${(efiDisplayGb / driveGb) * 100}%` }}
                    title={`EFI (${formatGb(efiDisplayGb)})`} />
                )}
                {/* Existing persistent partitions — only on a drive that is kept */}
                {(isExisting ? realLayout?.existingPartitions ?? [] : [])
                  .filter(p => p.fstype === 'ext4')
                  .map((p, i) => (
                    <div key={`real-${i}`} className="space-seg seg-persistent-blue"
                      style={{ width: `${(p.size_bytes / GIB / driveGb) * 100}%` }}
                      title={`${p.label || 'Existing'} (${formatGb(p.size_bytes / GIB)})`} />
                  ))
                }
                {/* Session-added partitions (persistent ISOs and reserved slots) */}
                {isos.filter(i => i.iso_type !== 'live' && !i.target_slot).map((iso, i) => (
                  <div key={`new-${i}`} className="space-seg seg-persistent-blue"
                    style={{
                      width: `${(iso.size_gb / driveGb) * 100}%`,
                      opacity: 0.7,
                      backgroundImage: 'repeating-linear-gradient(45deg, transparent, transparent 4px, rgba(255,255,255,0.1) 4px, rgba(255,255,255,0.1) 8px)',
                    }}
                    title={`${iso.name} (${iso.size_gb} GiB — to be written)`} />
                ))}
                {realFreeGb > 0.1 && (
                  <div className="space-seg seg-free-green"
                    style={{ width: `${(realFreeGb / driveGb) * 100}%` }}
                    title={`Free (${formatGb(realFreeGb)})`} />
                )}
              </div>
              <div className="space-available-label" style={{ marginTop: '0.5rem', marginBottom: 0 }}>
                Free space {isExisting ? 'remaining' : 'available'}: <strong>{formatGb(realFreeGb)}</strong>
              </div>
            </>
          )}
        </div>
      )}

      <div className="action-bar">
        <div />
        <button
          className="btn btn-primary"
          disabled={
            !device ||
            !!report?.drift ||
            (isExisting
              ? isos.length === 0
              : isos.filter(i => i.iso_type !== 'live').length === 0)
          }
          title={report?.drift ? 'Resolve the drive mismatch first — rebuild from disk or select another drive.' : undefined}
          onClick={onNext}
        >
          Continue →
        </button>
      </div>
    </div>
  );
}
