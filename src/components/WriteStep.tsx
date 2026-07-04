import { useEffect, useRef, useState } from 'react';
import type { AppState, WriteProgressEvent } from '../lib/types';
import { cancelOperation, onWriteProgress, runFullWrite, addPersistentIso, addLiveIso } from '../lib/api';

interface Props {
  state: AppState;
  isExisting: boolean;
  onDone: () => void;
}

type WriteState = 'writing' | 'done' | 'error';

interface LogLine {
  time: string;
  msg: string;
}

const STAGE_LABELS: Record<string, string> = {
  partition: 'Partitioning',
  format:    'Formatting',
  grub:      'Installing GRUB',
  extract:   'Extracting ISO',
  copy:      'Copying ISO',
  config:    'Writing config',
  sync:      'Syncing',
};

const STAGE_ORDER  = ['partition', 'format', 'grub', 'extract', 'copy', 'config', 'sync'];
const STAGE_WEIGHT: Record<string, number> = {
  partition: 3,
  format:    4,
  grub:      8,
  extract:   70,  // extraction dominates — unsquashfs + Fedora LiveOS copy are slow
  copy:      8,
  config:    3,
  sync:      4,
};

// Pure function — no closure over state. Calculates weighted overall percentage
// from the current stage and that stage's internal percentage.
function calcOverall(stage: string, stagePct: number): number {
  const idx = STAGE_ORDER.indexOf(stage);
  if (idx < 0) return 0;
  const done    = STAGE_ORDER.slice(0, idx).reduce((a, s) => a + (STAGE_WEIGHT[s] ?? 0), 0);
  const current = ((STAGE_WEIGHT[stage] ?? 0) * stagePct) / 100;
  const total   = STAGE_ORDER.reduce((a, s) => a + (STAGE_WEIGHT[s] ?? 0), 0);
  return Math.min(99, Math.round(((done + current) / total) * 100));
}

export function WriteStep({ state, isExisting, onDone }: Props) {
  const [writeState, setWriteState] = useState<WriteState>('writing');
  const [progress, setProgress]     = useState<WriteProgressEvent | null>(null);
  const [log, setLog]               = useState<LogLine[]>([]);
  const [error, setError]           = useState<string | null>(null);
  const [overallPct, setOverallPct] = useState(0);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let unlisten: (() => void) | undefined;

    onWriteProgress((evt) => {
      setProgress(evt);
      // calcOverall is a pure function — no stale closure over overallPct state
      setOverallPct(calcOverall(evt.stage, evt.pct));
      // Streaming progress re-emits the same message with a new pct —
      // move the bar, but don't repeat the log line.
      setLog(l =>
        l.length > 0 && l[l.length - 1].msg === evt.msg
          ? l
          : [...l, { time: new Date().toLocaleTimeString(), msg: evt.msg }]
      );
      requestAnimationFrame(() => {
        if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
      });
    }).then(fn => { unlisten = fn; });

    const startWrite = async () => {
      try {
        if (isExisting) {
          const dev = state.device!.path;
          for (const iso of state.isos) {
            if (iso.iso_type === 'persistent') {
              await addPersistentIso(dev, iso);
            } else {
              await addLiveIso(dev, iso);
            }
          }
        } else {
          await runFullWrite(
            state.device!.path,
            state.label,
            state.isos,
            state.free_space_gb,
          );
        }
        setOverallPct(100);
        setWriteState('done');
      } catch (e) {
        setError(String(e));
        setWriteState('error');
      }
    };

    startWrite();
    return () => { unlisten?.(); };
  }, []);

  const handleCancel = async () => {
    await cancelOperation();
    setWriteState('error');
    setError('Cancelled by user.');
  };

  // ── Writing ────────────────────────────────────────────────────────────────
  if (writeState === 'writing') {
    const stageLabel = progress
      ? (STAGE_LABELS[progress.stage] ?? progress.stage)
      : 'Starting…';

    return (
      <div className="step-layout">
        <div className="step-header">
          <h1>Writing…</h1>
          <p>Do not remove the USB drive. Extraction may take several minutes per ISO.</p>
        </div>

        <div className="card">
          <div className="progress-stage">{stageLabel}</div>
          <div className="progress-bar-track">
            <div className="progress-bar-fill" style={{ width: `${overallPct}%` }} />
          </div>
          <div className="progress-meta">
            <span>{progress?.msg ?? 'Preparing…'}</span>
            <span>{overallPct}%</span>
          </div>
        </div>

        <div className="card log-card">
          <div className="log-header">LOG</div>
          <div className="log-body" ref={logRef}>
            {log.map((l, i) => (
              <div key={i} className="log-line">
                <span className="log-time">{l.time}</span>
                <span className="log-msg">{l.msg}</span>
              </div>
            ))}
          </div>
        </div>

        <div className="action-bar">
          <button className="btn btn-danger" onClick={handleCancel}>✕ Cancel</button>
        </div>
      </div>
    );
  }

  // ── Done ───────────────────────────────────────────────────────────────────
  if (writeState === 'done') {
    const persistent = state.isos.filter(i => i.iso_type === 'persistent');
    return (
      <div className="step-layout center-content">
        <div className="done-icon">✓</div>
        <h1>USB is ready!</h1>
        <p>
          {persistent.length} persistent OS{persistent.length !== 1 ? 'es' : ''} written
          to {state.device?.model}. Each boots from its own partition — apps and files
          will persist across reboots.
        </p>
        <div className="warn-box" style={{ marginTop: '0.75rem' }}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"/>
            <line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>
          </svg>
          Before removing the drive, make sure it has been safely ejected. Right-click
          the drive in your file manager and select <strong>Safely Remove</strong> or <strong>Eject</strong>.
        </div>
        <button className="btn btn-primary" style={{ marginTop: '1.5rem' }} onClick={onDone}>
          Start over
        </button>
      </div>
    );
  }

  // ── Error ──────────────────────────────────────────────────────────────────
  return (
    <div className="step-layout center-content">
      <div className="error-icon">✕</div>
      <h1>Write failed</h1>
      <p className="error-msg">{error}</p>
      <div className="action-bar" style={{ justifyContent: 'center' }}>
        <button className="btn btn-primary" onClick={onDone}>← Start over</button>
      </div>
    </div>
  );
}
