import { useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { homeDir } from '@tauri-apps/api/path';
import { listDevices, getFileSize, getPartitionLayout, getIsoArch } from '../lib/api';
import type { IsoWriteSpec, BlockDevice, Partition, DriveReport } from '../lib/types';

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
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  return `${(bytes / 1e6).toFixed(0)} MB`;
}

function formatGb(gb: number): string {
  return `${gb % 1 === 0 ? gb : gb.toFixed(1)} GB`;
}

const EXTRACTED_MULTIPLIER = 2.2;
const HEADROOM = 1.1;

// Classify partitions from real drive layout
interface DriveLayout {
  efiGb: number;
  biosBootGb: number;
  existingPersistentGb: number;
  existingPartitions: Partition[];
  totalGb: number;
}

function parseDriveLayout(device: BlockDevice): DriveLayout {
  const totalGb = device.size_bytes / 1e9;
  let efiGb = 0;
  let biosBootGb = 0;
  let existingPersistentGb = 0;
  const existingPartitions: Partition[] = [];

  for (const p of device.children) {
    const labelUp = p.label.toUpperCase();
    const fsUp = p.fstype.toUpperCase();
    const gb = p.size_bytes / 1e9;

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

  // If no EFI found but drive has partitions, use 0.5 GB as fallback
  if (efiGb === 0 && device.children.length > 0) efiGb = 0.5;

  return { efiGb, biosBootGb, existingPersistentGb, existingPartitions, totalGb };
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
  onSave: (entry: IsoWriteSpec) => void;
  onCancel: () => void;
}

function ConfigureIso({
  entry, isNew, isos, editIdx, driveGb,
  existingPersistentGb, maxPersistentOverrideGb, onSave, onCancel,
}: ConfigureProps) {
  const [e, setE] = useState<IsoWriteSpec>({ ...entry });
  const [typeChosen, setTypeChosen] = useState(!isNew);

  const isPersistent = e.iso_type === 'persistent';

  // Live ISOs all share the free space partition. We track how many are already
  // queued and the total live bytes so we can show free-space awareness, but we
  // do NOT restrict to one — multiple live ISOs coexist, GRUB lists each.
  const sessionLiveCount = isos.filter((i, idx) => i.iso_type === 'live' && idx !== editIdx).length;
  const sessionLiveBytes = isos
    .filter((i, idx) => i.iso_type === 'live' && idx !== editIdx)
    .reduce((s, i) => s + i.file_size_bytes, 0);

  const efiGb = 0.5;
  const sessionPersistentGb = isos
    .filter((_, idx) => idx !== editIdx)
    .filter(i => i.iso_type === 'persistent')
    .reduce((s, i) => s + i.size_gb, 0);

  const totalAllocated = efiGb + existingPersistentGb + sessionPersistentGb;
  // On an existing drive, a new persistent partition is carved from the real
  // FREESPACE partition, so the ceiling is that free space minus what this
  // session already queued — not the whole-drive arithmetic.
  const availableGb = maxPersistentOverrideGb !== undefined
    ? Math.max(0, maxPersistentOverrideGb)
    : Math.max(0, driveGb - totalAllocated);

  const extractedGb = (e.file_size_bytes / 1e9) * EXTRACTED_MULTIPLIER * HEADROOM;
  const minSizeGb = Math.max(1, Math.ceil(extractedGb) + 1);
  const maxSizeGb = Math.max(minSizeGb + 1, Math.floor(availableGb));
  const clampedSize = Math.min(Math.max(e.size_gb, minSizeGb), maxSizeGb);
  const remainingAfter = Math.max(0, availableGb - clampedSize);

  const chooseType = (t: 'persistent' | 'live') => {
    setE({ ...e, iso_type: t });
    setTypeChosen(true);
  };

  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>{isNew ? 'Add ISO' : 'Edit ISO'}</h1>
      </div>

      <div className="card form-card">
        <label className="field">
          <span>Display name</span>
          <input
            value={e.name}
            onChange={ev => setE({ ...e, name: ev.target.value })}
            placeholder="e.g. Linux Mint 22"
          />
        </label>

        <div className="field">
          <span>Type</span>
          <div className="field-hint" style={{ marginBottom: '0.4rem' }}>
            Select the Drive Type below to Continue
          </div>
          <div className="type-grid">
            <button
              type="button"
              className={`type-card ${typeChosen && !isPersistent ? 'type-active' : ''} ${!typeChosen ? 'type-unset' : ''}`}
              onClick={() => chooseType('live')}
            >
              <div className="type-radio">
                <div className={`radio-dot ${typeChosen && !isPersistent ? 'on' : ''}`} />
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
          {typeChosen && !isPersistent && (
            <div className="info-panel">
              Live ISOs share the free space partition — you can add several and
              pick which to boot from the GRUB menu. They run without persistence
              (changes are lost on reboot).
              {sessionLiveCount > 0 &&
                ` ${sessionLiveCount} live ISO${sessionLiveCount > 1 ? 's' : ''} already queued (${formatBytes(sessionLiveBytes)}).`}
            </div>
          )}
        </div>

        {typeChosen && isPersistent && (
          <div className="field">
            <span>Partition size: <strong>{formatGb(clampedSize)}</strong></span>
            <input
              type="range"
              min={minSizeGb}
              max={maxSizeGb}
              value={clampedSize}
              onChange={ev => setE({ ...e, size_gb: Number(ev.target.value) })}
            />
            <div className="field-hint">
              Minimum {minSizeGb} GB (extracted OS + headroom).
              {availableGb > 0 && ` ${formatGb(availableGb)} available on this drive.`}
              {remainingAfter > 0.5 && ` ${formatGb(remainingAfter)} will remain as free space.`}
            </div>
          </div>
        )}

        <div className="field">
          <span>ISO file</span>
          <input value={e.path} readOnly className="input-readonly" />
          {e.file_size_bytes > 0 && (
            <div className="field-hint">
              {formatBytes(e.file_size_bytes)}
              {typeChosen && isPersistent && ` · Estimated extracted: ~${(e.file_size_bytes / 1e9 * EXTRACTED_MULTIPLIER).toFixed(1)} GB`}
            </div>
          )}
        </div>

        {typeChosen && isPersistent && (
          <>
            <label className="field">
              <span>Partition label</span>
              <input
                value={e.label}
                maxLength={11}
                onChange={ev => setE({ ...e, label: ev.target.value.toUpperCase().replace(/[^A-Z0-9]/g, '') })}
                placeholder="MINT223"
              />
            </label>
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
                onChange={ev => setE({ ...e, username: ev.target.value.replace(/\s/g, '') })}
                placeholder="e.g. mint"
                autoComplete="off"
              />
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
            !e.name ||
            (isPersistent && (!e.username || !e.password))
          }
          onClick={() => onSave({ ...e, size_gb: clampedSize })}
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
        totalGb: dev.size_bytes / 1e9,
      });
    }
    setLayoutLoading(false);
  };

  const driveGb = realLayout?.totalGb ?? (device?.size_bytes ?? 0) / 1e9;

  // On an existing Boot OS Pro drive, the real free space is the FREESPACE
  // partition's actual size (from the manifest) minus whatever live ISOs the
  // user has queued this session. New persistent adds carve from this, so the
  // sliders must be bounded by it — never by whole-drive arithmetic.
  const manifestFreeGb = report?.manifest
    ? report.manifest.free_space.size_bytes / 1e9
    : 0;
  const queuedLiveGb = isos
    .filter(i => i.iso_type === 'live')
    .reduce((s, i) => s + i.file_size_bytes / 1e9, 0);
  const queuedNewPersistentGb = isos
    .filter(i => i.iso_type === 'persistent')
    .reduce((s, i) => s + i.size_gb, 0);
  const realFreeGb = isExisting
    ? Math.max(0, manifestFreeGb - queuedLiveGb - queuedNewPersistentGb)
    : freeSpaceGb;

  // Space graphic values — always read from real layout + session additions
  const realEfiGb        = realLayout?.efiGb ?? 0;
  const realPersistentGb = realLayout?.existingPersistentGb ?? 0;
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
    try { file_size_bytes = await getFileSize(path); } catch { /* non-fatal */ }
    try { arch = await getIsoArch(path); } catch { /* non-fatal */ }
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

    const extractedGb = (file_size_bytes / 1e9) * EXTRACTED_MULTIPLIER * HEADROOM;
    const defaultSizeGb = Math.max(Math.ceil(extractedGb) + 1, 1);

    const entry: IsoWriteSpec = {
      name: filename.replace(/\.[^.]+$/, '').replace(/[-_]/g, ' '),
      path,
      label: makeLabel(filename.replace(/\.[^.]+$/, ''), isos),
      size_gb: defaultSizeGb,
      boot_params: BOOT_PARAMS[distro] ?? BOOT_PARAMS.custom,
      file_size_bytes,
      iso_type: 'persistent',
      username: '',
      password: '',
    };
    setConfiguring(entry);
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
                  <span className="content-meta">persistent · {formatGb(p.size_bytes / 1e9)}</span>
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
                {iso.path.split('/').pop()}
                {iso.file_size_bytes > 0 && ` · ${formatBytes(iso.file_size_bytes)}`}
              </div>
              <div className="iso-badges">
                {iso.iso_type === 'persistent' ? (
                  <>
                    <span className="badge badge-blue">persistent</span>
                    <span className="badge badge-blue-dim">{iso.size_gb} GB</span>
                    <span className="badge badge-gray">{iso.label}</span>
                  </>
                ) : (
                  <span className="badge badge-green">live session</span>
                )}
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
                {/* Existing persistent partitions on drive */}
                {(realLayout?.existingPartitions ?? [])
                  .filter(p => p.fstype === 'ext4')
                  .map((p, i) => (
                    <div key={`real-${i}`} className="space-seg seg-persistent-blue"
                      style={{ width: `${(p.size_bytes / 1e9 / driveGb) * 100}%` }}
                      title={`${p.label || 'Existing'} (${formatGb(p.size_bytes / 1e9)})`} />
                  ))
                }
                {/* Session-added persistent partitions */}
                {isos.filter(i => i.iso_type === 'persistent').map((iso, i) => (
                  <div key={`new-${i}`} className="space-seg seg-persistent-blue"
                    style={{
                      width: `${(iso.size_gb / driveGb) * 100}%`,
                      opacity: 0.7,
                      backgroundImage: 'repeating-linear-gradient(45deg, transparent, transparent 4px, rgba(255,255,255,0.1) 4px, rgba(255,255,255,0.1) 8px)',
                    }}
                    title={`${iso.name} (${iso.size_gb} GB — to be written)`} />
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
              : isos.filter(i => i.iso_type === 'persistent').length === 0)
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
