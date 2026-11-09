// Boot OS Pro — SummaryStep.tsx — build 1.0.2 — Monday 9 November 2026

import type { AppState, DriveReport } from '../lib/types';
import { GIB, MIB, INSTALLER_MIB } from '../lib/units';
import { installedVersion } from '../lib/api';
import { getVersion } from '@tauri-apps/api/app';
import { message } from '@tauri-apps/plugin-dialog';

/** True when a different version is installed than the one running. Any
 *  failure to read either version lets the write go ahead. */
async function newerInstalled(): Promise<boolean> {
  try {
    const [running, installed] = await Promise.all([getVersion(), installedVersion()]);
    return installed !== '' && installed !== running;
  } catch {
    return false;
  }
}

interface Props {
  state: AppState;
  report: DriveReport | null;
  isExisting: boolean;
  onBack: () => void;
  onNext: () => void;
}

function formatGb(gb: number): string {
  return `${gb % 1 === 0 ? gb : gb.toFixed(1)} GiB`;
}

function formatBytes(bytes: number): string {
  if (bytes >= GIB) return `${(bytes / GIB).toFixed(1)} GiB`;
  return `${(bytes / MIB).toFixed(0)} MiB`;
}

const HATCH = 'repeating-linear-gradient(45deg, transparent, transparent 4px, rgba(255,255,255,0.18) 4px, rgba(255,255,255,0.18) 8px)';

export function SummaryStep({ state, report, isExisting, onBack, onNext }: Props) {
  const { device, isos, free_space_gb } = state;

  const handleWrite = async () => {
    if (await newerInstalled()) {
      await message(
        'A new version has been installed. Please restart the application for the new features to take effect.',
        { title: 'Boot OS Pro', kind: 'info' },
      );
      return;
    }
    onNext();
  };
  const driveGb = (device?.size_bytes ?? 0) / GIB;

  // New partitions: persistent ISOs carved or created, and reserved slots.
  // A persistent ISO with a target slot installs into an existing empty slot.
  const newPersistent = isos.filter(i => i.iso_type !== 'live' && !i.target_slot);
  const slotFills     = isos.filter(i => i.iso_type === 'persistent' && !!i.target_slot);
  const newLive       = isos.filter(i => i.iso_type === 'live');

  // Free space filesystem: exFAT on new drives; on an existing drive, what
  // the FREESPACE partition really is (FAT32 on drives made before 1.0.19).
  const freePart = device?.children.find(p => p.label.toUpperCase() === 'FREESPACE');
  const freeFs = !isExisting || freePart?.fstype.toLowerCase() === 'exfat' ? 'exFAT'
    : freePart?.fstype.toLowerCase() === 'vfat' ? 'FAT32' : (freePart?.fstype ?? 'exFAT');

  // Existing contents come from the manifest (only present on an existing drive).
  const existingPersistent = isExisting ? (report?.manifest?.persistent ?? []) : [];
  const existingLive        = isExisting ? (report?.manifest?.live ?? []) : [];
  // A persistent add re-creates the free space partition, removing the live
  // ISOs already on it — only show them as kept when nothing is carved.
  const keptLive = newPersistent.length > 0 ? [] : existingLive;
  const manifestFreeGb      = isExisting ? (report?.manifest?.free_space.size_bytes ?? 0) / GIB : 0;

  const newPersistentGb = newPersistent.reduce((s, i) => s + i.size_gb, 0);
  const newLiveGb       = newLive.reduce((s, i) => s + i.file_size_bytes / GIB, 0);

  // Free space remaining after this operation.
  const remainingFreeGb = isExisting
    ? Math.max(0, manifestFreeGb - newPersistentGb - newLiveGb)
    : free_space_gb;
  const freeSpaceLow = remainingFreeGb > 0 && remainingFreeGb < 4 && newLive.length > 0;

  const efiGb = 512 / 1024;
  const installerGb = INSTALLER_MIB / 1024;

  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>{isExisting ? 'Confirm changes' : 'Review drive layout'}</h1>
        <p>
          {isExisting
            ? <>Adding to <strong>{device?.model}</strong> ({device?.path}). Existing contents are kept; new items are added to the free space.</>
            : <>This is what will be written to <strong>{device?.model}</strong> ({device?.path}). The drive will be erased and created from scratch.</>}
        </p>
      </div>

      {/* Resulting drive map */}
      <div className="card">
        {isExisting && (
          <div className="space-key" style={{ marginBottom: '0.5rem' }}>
            <span><span className="key-dot" style={{ background: 'var(--text-secondary)' }} /> Existing (kept)</span>
            <span><span className="key-dot" style={{ background: 'var(--accent)', backgroundImage: HATCH }} /> Being added</span>
          </div>
        )}
        <div className="summary-drive-map">
          {/* EFI — always present */}
          <div className="drive-map-row">
            <div className="drive-map-seg seg-efi-red"
              style={{ width: `${(efiGb / driveGb) * 100}%`, minWidth: '3%' }} />
            <div className="drive-map-label">
              <span className="label-name">EFI</span>
              <span className="label-size">512 MiB · FAT32</span>
            </div>
          </div>

          {/* BOOTOSPRO — app copy and drivers (new drives) */}
          {!isExisting && (
            <div className="drive-map-row">
              <div className="drive-map-seg seg-efi-red"
                style={{ width: `${(installerGb / driveGb) * 100}%`, minWidth: '3%', opacity: 0.7 }} />
              <div className="drive-map-label">
                <span className="label-name">BOOTOSPRO</span>
                <span className="label-size">{INSTALLER_MIB} MiB · FAT32 · app copy and drivers</span>
              </div>
            </div>
          )}

          {/* Existing persistent (kept) */}
          {existingPersistent.map((p, i) => {
            const fill = slotFills.find(f => f.target_slot === p.label);
            return (
            <div key={`ep-${i}`} className="drive-map-row">
              <div className="drive-map-seg seg-persistent-blue"
                style={{ width: `${(p.size_bytes / GIB / driveGb) * 100}%`, minWidth: '3%',
                         backgroundImage: fill ? HATCH : undefined }} />
              <div className="drive-map-label">
                <span className="label-name">
                  {fill
                    ? <>{fill.name} <span className="tag-new">into {p.label}</span></>
                    : <>{p.state === 'filled' ? (p.os_name || p.label) : `${p.label} — empty slot`}
                        <span className="tag-kept">kept</span></>}
                </span>
                <span className="label-size">{formatGb(p.size_bytes / GIB)} · ext4 · {fill ? fill.label : p.label}</span>
              </div>
            </div>
            );
          })}

          {/* New persistent (being added) */}
          {newPersistent.map((iso, i) => (
            <div key={`np-${i}`} className="drive-map-row">
              <div className="drive-map-seg seg-persistent-blue"
                style={{ width: `${(iso.size_gb / driveGb) * 100}%`, minWidth: '3%', backgroundImage: HATCH }} />
              <div className="drive-map-label">
                <span className="label-name">
                  {iso.iso_type === 'reserved' ? `${iso.label} — empty slot` : iso.name}
                  {isExisting && <span className="tag-new">new</span>}
                </span>
                <span className="label-size">
                  {formatGb(iso.size_gb)} · ext4 · {iso.label}
                  {iso.file_size_bytes > 0 && ` · ISO ${formatBytes(iso.file_size_bytes)}`}
                </span>
              </div>
            </div>
          ))}

          {/* Free space — holds existing + new live ISOs */}
          {(isExisting ? manifestFreeGb : free_space_gb) > 0 && (
            <div className="drive-map-row">
              <div className="drive-map-seg seg-free-green"
                style={{ width: `${((isExisting ? manifestFreeGb : free_space_gb) / driveGb) * 100}%`, minWidth: '3%' }} />
              <div className="drive-map-label">
                <span className="label-name">Free Space</span>
                <span className="label-size">
                  {formatGb(isExisting ? manifestFreeGb : free_space_gb)} · {freeFs} · live ISOs
                </span>
              </div>
            </div>
          )}
        </div>

        {/* Live ISOs in free space — listed below the map */}
        {(keptLive.length > 0 || newLive.length > 0) && (
          <div className="summary-live-list">
            <div className="section-label" style={{ marginTop: '0.6rem' }}>Live ISOs in free space</div>
            {keptLive.map((l, i) => (
              <div key={`el-${i}`} className="content-row">
                <span className="content-dot key-free" />
                <span className="content-name">{l.os_name} <span className="tag-kept">kept</span></span>
                <span className="content-meta">{formatBytes(l.size_bytes)}</span>
              </div>
            ))}
            {newLive.map((l, i) => (
              <div key={`nl-${i}`} className="content-row">
                <span className="content-dot key-free" style={{ backgroundImage: HATCH }} />
                <span className="content-name">{l.name} {isExisting && <span className="tag-new">new</span>}</span>
                <span className="content-meta">{formatBytes(l.file_size_bytes)}</span>
              </div>
            ))}
          </div>
        )}

        <div className="summary-totals">
          <span>Free space {isExisting ? 'after changes' : 'on drive'}: {formatGb(remainingFreeGb)}</span>
          <span>Drive: {formatGb(driveGb)}</span>
        </div>
      </div>

      {/* Warnings */}
      {isExisting && newPersistent.length > 0 && existingLive.length > 0 && (
        <div className="warn-box">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
            <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
          </svg>
          Adding a persistent OS re-creates the free space partition. The{' '}
          {existingLive.length} live ISO{existingLive.length > 1 ? 's' : ''} already on this
          drive will be removed. Live ISOs added in this session are kept.
        </div>
      )}
      {freeSpaceLow && (
        <div className="warn-box">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
            <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
          </svg>
          Free space will be under 4 GiB after this — further live ISOs may not fit.
        </div>
      )}

      {isExisting ? (
        <div className="info-panel" style={{ marginBottom: '1rem' }}>
          Existing partitions and their data are not touched. Only the new items
          above are written, using the free space.
        </div>
      ) : (
        <div className="warn-box" style={{ marginBottom: '1rem' }}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
            <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
          </svg>
          All data on {device?.path} will be permanently erased and the drive
          recreated. Make sure you have selected the correct device.
        </div>
      )}

      <div className="action-bar">
        <button className="btn" onClick={onBack}>← Back</button>
        <button className="btn btn-danger" onClick={handleWrite}>
          {isExisting ? '⚡ Apply changes' : '⚡ Write to USB'}
        </button>
      </div>
    </div>
  );
}
