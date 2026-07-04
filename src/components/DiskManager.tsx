import { useState, useEffect } from 'react';
import type { BlockDevice, Partition } from '../lib/types';
import {
  listDevices, getPartitionLayout, formatPartition,
  deletePartition, wipeDevice, onWriteProgress,
  formatPersistentSlot, formatFreeSpaceSlot, unmountPartition,
} from '../lib/api';

type Action = 'format' | 'delete' | 'wipe' | 'empty-slot' | 'clear-free' | null;
type OpState = 'idle' | 'busy' | 'done' | 'error';

const FS_TYPES = ['exfat', 'fat32', 'ext4', 'ntfs'];

// Map an lsblk fstype to the dropdown's value so Format defaults to the
// partition's CURRENT format — never silently converting a FAT32 free space
// partition to ext4, or vice versa. lsblk reports FAT as "vfat".
function defaultFsFor(fstype: string): string {
  const f = fstype.toLowerCase();
  if (f === 'vfat' || f === 'fat' || f === 'fat32') return 'fat32';
  if (f === 'ext4' || f === 'ext2' || f === 'ext3') return 'ext4';
  if (f === 'exfat') return 'exfat';
  if (f === 'ntfs') return 'ntfs';
  return 'fat32'; // unformatted/unknown → safest general default
}

const FS_COLOR: Record<string, string> = {
  vfat: '#3b82f6', fat32: '#3b82f6', exfat: '#8b5cf6',
  ext4: '#f59e0b', ntfs: '#10b981', '': 'var(--border-strong)',
};

const STAGE_LABELS: Record<string, string> = {
  partition: 'Partitioning', format: 'Formatting', grub: 'Installing GRUB',
  extract: 'Extracting', copy: 'Copying', config: 'Writing config', sync: 'Syncing',
};

export function DiskManager() {
  const [devices, setDevices]       = useState<BlockDevice[]>([]);
  const [selected, setSelected]     = useState<BlockDevice | null>(null);
  const [layout, setLayout]         = useState<BlockDevice | null>(null);
  const [activePart, setActivePart] = useState<Partition | null>(null);
  const [action, setAction]         = useState<Action>(null);
  const [fstype, setFstype]         = useState('exfat');
  const [partLabel, setPartLabel]   = useState('');
  const [wipeLabel, setWipeLabel]   = useState('USB');
  const [wipeFs, setWipeFs]         = useState('fat32');
  const [opState, setOpState]       = useState<OpState>('idle');
  const [stageLabel, setStageLabel] = useState('');
  const [doneMsg, setDoneMsg]       = useState('');
  const [error, setError]           = useState<string | null>(null);

  useEffect(() => {
    listDevices().then(setDevices).catch(() => setDevices([]));
  }, []);

  const refreshLayout = async (dev: BlockDevice) => {
    setSelected(dev); setActivePart(null); setAction(null); setError(null);
    try { setLayout(await getPartitionLayout(dev.path)); }
    catch (e) { setError(String(e)); }
  };

  const dismiss = async () => {
    setOpState('idle');
    setDoneMsg('');
    setStageLabel('');
    if (selected) await refreshLayout(selected);
  };

  const run = async (label: string, successMsg: string, fn: () => Promise<void>) => {
    setStageLabel(label);
    setOpState('busy');
    setError(null);
    // Yield to let React render the empty bar before operation fires
    await new Promise(r => setTimeout(r, 50));

    const unlisten = await onWriteProgress(evt => {
      setStageLabel(STAGE_LABELS[evt.stage] ?? evt.stage);
    });

    try {
      await fn();
      unlisten();
      // Show full blue bar — stay until user dismisses
      setDoneMsg(successMsg);
      setOpState('done');
    } catch (e) {
      unlisten();
      setError(String(e));
      setOpState('error');
    }
  };

  const handleFormat = () => {
    if (!activePart) return;
    const target = activePart.path;
    const fs = fstype;
    const lbl = partLabel || 'USB';
    run(
      `Formatting ${target} as ${fs}…`,
      `${target} formatted as ${fs}.`,
      () => formatPartition(target, fs, lbl)
    );
  };

  const handleDelete = () => {
    if (!activePart || !selected) return;
    const target = activePart.path;
    run(
      `Deleting ${target}…`,
      `${target} deleted.`,
      () => deletePartition(selected.path, target)
    );
  };

  const handleWipe = () => {
    if (!selected) return;
    const dev = selected.path;
    const lbl = wipeLabel || 'USB';
    run(
      `Wiping ${dev}…`,
      `${dev} wiped and formatted as ${wipeFs}.`,
      () => wipeDevice(dev, lbl, wipeFs)
    );
  };

  // Scalpel: format one persistent OS partition in place, leaving the others
  // intact. The slot becomes an empty persistent slot and its boot entry is
  // dropped — the rest of the drive keeps booting.
  const handleFormatPersistent = () => {
    if (!activePart || !selected) return;
    const target = activePart.path;
    const lbl = activePart.label || 'EMPTY';
    run(
      `Emptying ${target}…`,
      `${target} is now an empty persistent slot, ready for a new OS.`,
      () => formatPersistentSlot(selected.path, target, lbl)
    );
  };

  // Format the free space partition — removes ALL live ISOs at once.
  const handleFormatFreeSpace = () => {
    if (!selected) return;
    run(
      'Clearing free space — all live ISOs will be removed…',
      'All live ISOs removed.',
      () => formatFreeSpaceSlot(selected.path)
    );
  };

  // Unmount a mounted partition in place, then refresh so the format/delete
  // actions unlock. Auto-mounted removable partitions block formatting.
  const handleUnmount = (part: Partition) => {
    run(
      `Unmounting ${part.path}…`,
      `${part.path} unmounted.`,
      () => unmountPartition(part.path),
    );
  };

  // Classify the selected partition so the right action is offered.
  const activeIsPersistent =
    !!activePart && activePart.fstype === 'ext4' &&
    activePart.label.toUpperCase() !== 'BOOTOSPRO';
  const activeIsFreeSpace =
    !!activePart && activePart.label.toUpperCase() === 'FREESPACE';

  const totalBytes = layout?.size_bytes ?? 1;
  const usedBytes  = layout?.children.reduce((s, p) => s + p.size_bytes, 0) ?? 0;
  const freeBytes  = Math.max(0, totalBytes - usedBytes);
  const busy       = opState === 'busy';

  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>Disk Manager</h1>
        <p>Format, delete, or wipe partitions on a USB drive.</p>
      </div>

      {/* Device selector */}
      <div className="card">
        <div className="section-label">Select device</div>
        <div style={{ display: 'flex', gap: '0.5rem', flexWrap: 'wrap', alignItems: 'center' }}>
          {devices.length === 0 && (
            <span style={{ color: 'var(--text-secondary)', fontSize: '13px' }}>No USB devices found.</span>
          )}
          {devices.map(d => (
            <button key={d.path}
              className={`btn ${selected?.path === d.path ? 'btn-primary' : ''}`}
              onClick={() => refreshLayout(d)}
            >
              {d.model} ({d.path}) — {d.size_human}
            </button>
          ))}
          <button className="btn" onClick={() => listDevices().then(setDevices)}>↺ Refresh</button>
        </div>
      </div>

      {/* Progress / done bar — shown during and after operation until dismissed */}
      {(opState === 'busy' || opState === 'done') && (
        <div className="card">
          <div className="progress-stage">{opState === 'done' ? doneMsg : stageLabel}</div>
          <div className="progress-bar-track">
            <div
              className="progress-bar-fill"
              style={{
                width: opState === 'done' ? '100%' : '60%',
                background: opState === 'done' ? '#16a34a' : 'var(--accent)',
                transition: opState === 'done' ? 'width 0.3s ease' : 'none',
              }}
            />
          </div>
          {opState === 'done' && (
            <div style={{ display: 'flex', justifyContent: 'flex-end', marginTop: '0.5rem' }}>
              <button className="btn btn-primary" onClick={dismiss}>OK</button>
            </div>
          )}
          {opState === 'busy' && (
            <div className="progress-meta">
              <span>{stageLabel}</span>
              <span>Working…</span>
            </div>
          )}
        </div>
      )}

      {opState === 'error' && error && (
        <div className="warn-box" style={{ marginBottom: '0.5rem' }}>
          {error}
          <button className="btn" style={{ marginLeft: '1rem' }} onClick={dismiss}>Dismiss</button>
        </div>
      )}

      {/* Partition layout — hidden while busy or done */}
      {layout && opState === 'idle' && (
        <>
          <div className="card">
            <div className="section-label">Partition layout — {layout.size_human}</div>
            <div className="partition-vis" style={{ height: '32px' }}>
              {layout.children.map((p, i) => (
                <div key={i}
                  className={`part-seg ${activePart?.path === p.path ? 'active' : ''}`}
                  style={{
                    flex: p.size_bytes,
                    background: activePart?.path === p.path
                      ? 'var(--accent)'
                      : (FS_COLOR[p.fstype] ?? 'var(--border-strong)'),
                    cursor: 'pointer',
                    outline: activePart?.path === p.path ? '2px solid var(--accent)' : 'none',
                  }}
                  onClick={() => { setActivePart(p); setAction(null); setError(null); }}
                  title={`${p.path} — ${p.fstype || 'unknown'} — ${(p.size_bytes / 1e9).toFixed(1)} GB`}
                >
                  <span style={{ fontSize: '0.65rem' }}>{(p.size_bytes / 1e9).toFixed(1)}G</span>
                </div>
              ))}
              {freeBytes > 1e8 && (
                <div className="part-seg"
                  style={{ flex: freeBytes, background: 'var(--bg)', border: '1px dashed var(--border)' }}
                  title="Unallocated"
                >
                  <span style={{ fontSize: '0.65rem', color: 'var(--text-tertiary)' }}>free</span>
                </div>
              )}
            </div>

            <div style={{ marginTop: '0.5rem' }}>
              {layout.children.map((p, i) => (
                <div key={i}
                  className={`dm-part-row ${activePart?.path === p.path ? 'dm-part-active' : ''}`}
                  onClick={() => { setActivePart(p); setAction(null); setError(null); }}
                >
                  <div className="dm-part-info">
                    <span className="dm-part-path">{p.path}</span>
                    <span className="dm-part-meta">
                      {p.fstype || 'unformatted'} · {(p.size_bytes / 1e9).toFixed(1)} GB
                      {p.label && ` · ${p.label}`}
                      {p.mountpoint && ` · 📌 ${p.mountpoint}`}
                    </span>
                  </div>
                  {activePart?.path === p.path && (
                    <div style={{ display: 'flex', gap: '4px' }}>
                      {p.mountpoint && (
                        <button className="btn" style={{ padding: '4px 10px', fontSize: '12px' }}
                          disabled={busy}
                          title={`Unmount ${p.mountpoint}`}
                          onClick={ev => { ev.stopPropagation(); handleUnmount(p); }}>
                          Unmount
                        </button>
                      )}
                      {activeIsPersistent && (
                        <button className="btn" style={{ padding: '4px 10px', fontSize: '12px' }}
                          disabled={busy}
                          title="Empty this OS slot, keep the partition"
                          onClick={ev => { ev.stopPropagation(); setAction('empty-slot'); }}>
                          Empty Slot
                        </button>
                      )}
                      {activeIsFreeSpace && (
                        <button className="btn" style={{ padding: '4px 10px', fontSize: '12px' }}
                          disabled={busy}
                          title="Remove all live ISOs"
                          onClick={ev => { ev.stopPropagation(); setAction('clear-free'); }}>
                          Clear Live ISOs
                        </button>
                      )}
                      <button className="btn" style={{ padding: '4px 10px', fontSize: '12px' }}
                        disabled={busy}
                        onClick={ev => { ev.stopPropagation(); setFstype(defaultFsFor(p.fstype)); setAction('format'); }}>
                        Format
                      </button>
                      <button className="btn btn-danger" style={{ padding: '4px 10px', fontSize: '12px' }}
                        disabled={busy || !!p.mountpoint}
                        title={p.mountpoint ? 'Unmount first' : ''}
                        onClick={ev => { ev.stopPropagation(); setAction('delete'); }}>
                        Delete
                      </button>
                    </div>
                  )}
                </div>
              ))}
            </div>
          </div>

          {action === 'format' && activePart && (
            <div className="card">
              <div className="section-label">Format {activePart.path}</div>
              <div className="warn-box" style={{ marginBottom: '0.5rem' }}>
                Warning: Persistent Drives on this disc require Ext4 format. Free
                Space drives require Fat32 format. Please select accordingly when
                re-formatting.
              </div>
              {activePart.mountpoint && (
                <div className="warn-box" style={{ marginBottom: '0.5rem' }}>
                  Partition mounted at {activePart.mountpoint} — unmount before formatting.
                </div>
              )}
              <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center', flexWrap: 'wrap' }}>
                <select value={fstype} onChange={e => setFstype(e.target.value)}
                  className="text-input" style={{ width: 'auto' }}>
                  {FS_TYPES.map(f => <option key={f} value={f}>{f}</option>)}
                </select>
                <input className="text-input" placeholder="Label (optional)"
                  value={partLabel} maxLength={11}
                  onChange={e => setPartLabel(e.target.value.toUpperCase())}
                  style={{ width: '11rem' }} />
              </div>
              <div className="warn-box" style={{ marginTop: '0.5rem' }}>
                All data on {activePart.path} will be erased.
              </div>
              <div style={{ display: 'flex', gap: '8px', marginTop: '0.5rem', justifyContent: 'flex-end' }}>
                <button className="btn" onClick={() => setAction(null)}>Cancel</button>
                <button className="btn btn-danger"
                  disabled={busy || !!activePart.mountpoint}
                  onClick={handleFormat}>
                  Format
                </button>
              </div>
            </div>
          )}

          {action === 'delete' && activePart && (
            <div className="card">
              <div className="section-label">Delete {activePart.path}</div>
              <div className="warn-box">Permanently deletes the partition and all its data.</div>
              <div style={{ display: 'flex', gap: '8px', marginTop: '0.5rem', justifyContent: 'flex-end' }}>
                <button className="btn" onClick={() => setAction(null)}>Cancel</button>
                <button className="btn btn-danger" disabled={busy} onClick={handleDelete}>
                  Delete partition
                </button>
              </div>
            </div>
          )}

          {action === 'empty-slot' && activePart && (
            <div className="card">
              <div className="section-label">Empty OS slot — {activePart.path}</div>
              <div className="info-panel">
                Reformats just this partition and marks it an empty persistent slot.
                The other persistent OSes and their boot menu entries are untouched —
                only this one's data is erased and its entry removed. Install a new OS
                into the empty slot afterwards.
              </div>
              <div style={{ display: 'flex', gap: '8px', marginTop: '0.5rem', justifyContent: 'flex-end' }}>
                <button className="btn" onClick={() => setAction(null)}>Cancel</button>
                <button className="btn btn-danger" disabled={busy} onClick={handleFormatPersistent}>
                  Empty this slot
                </button>
              </div>
            </div>
          )}

          {action === 'clear-free' && activePart && (
            <div className="card">
              <div className="section-label">Clear live ISOs — {activePart.path}</div>
              <div className="warn-box">
                The free space partition holds every live ISO on this drive. Clearing
                it removes them all at once — there is no per-ISO removal. Persistent
                OSes are not affected.
              </div>
              <div style={{ display: 'flex', gap: '8px', marginTop: '0.5rem', justifyContent: 'flex-end' }}>
                <button className="btn" onClick={() => setAction(null)}>Cancel</button>
                <button className="btn btn-danger" disabled={busy} onClick={handleFormatFreeSpace}>
                  Remove all live ISOs
                </button>
              </div>
            </div>
          )}

          <div className="card">
            <div className="section-label">Wipe entire device</div>
            <p style={{ color: 'var(--text-secondary)', fontSize: '13px', margin: '0 0 0.5rem' }}>
              Erases everything and creates a single partition. fat32 works on
              Linux, Windows, and macOS; choose exfat for files over 4 GB.
            </p>
            <div style={{ display: 'flex', gap: '0.5rem', alignItems: 'center' }}>
              <input className="text-input" placeholder="Label"
                value={wipeLabel} maxLength={11}
                onChange={e => setWipeLabel(e.target.value.toUpperCase())}
                style={{ width: '9rem' }} />
              <select className="text-input" value={wipeFs}
                onChange={e => setWipeFs(e.target.value)}
                style={{ width: '7rem' }}>
                {FS_TYPES.map(f => <option key={f} value={f}>{f}</option>)}
              </select>
              <button className="btn btn-danger" disabled={busy}
                onClick={() => setAction('wipe')}>
                Wipe device
              </button>
            </div>
            {action === 'wipe' && (
              <div className="warn-box" style={{ marginTop: '0.5rem' }}>
                <div style={{ flex: 1 }}>
                  All data on <strong>{selected?.path}</strong> will be permanently erased.
                </div>
                <div style={{ display: 'flex', gap: '6px', flexShrink: 0 }}>
                  <button className="btn" style={{ padding: '4px 10px', fontSize: '12px' }}
                    onClick={() => setAction(null)}>Cancel</button>
                  <button className="btn btn-danger" style={{ padding: '4px 10px', fontSize: '12px' }}
                    disabled={busy} onClick={handleWipe}>
                    Confirm
                  </button>
                </div>
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}
