import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import type { AppState, Step, DriveReport } from './lib/types';
import { readDriveManifest, reconcileManifest } from './lib/api';
import { StepBar } from './components/StepBar';
import { ImagesStep } from './components/ImagesStep';
import { SummaryStep } from './components/SummaryStep';
import { WriteStep } from './components/WriteStep';
import { DiskManager } from './components/DiskManager';
import './app.css';

type AppMode = 'create' | 'manage' | 'about';

const INITIAL_STATE: AppState = {
  step: 'configure',
  device: null,
  isos: [],
  free_space_gb: 0,
  label: 'BOOTOSPRO',
};

function applyTheme(dark: boolean) {
  const t = dark ? 'dark' : 'light';
  // Set on BOTH html and body. The dark variable block in app.css is declared
  // unconditionally on body (WebKit2GTK :root cascade workaround) — per the
  // CSS spec, body's own dark values would beat the light override inherited
  // from html[data-theme="light"]. Tagging body makes the light selector
  // match body directly, so this is spec-correct instead of quirk-dependent.
  document.documentElement.setAttribute('data-theme', t);
  document.body.setAttribute('data-theme', t);
}

function AboutPanel({ version }: { version: string }) {
  return (
    <div className="step-layout">
      <div className="step-header">
        <h1>Boot OS Pro</h1>
        <p>Multiboot USB Creator with Persistent Storage</p>
      </div>

      <div className="card">
        <div className="summary-card">
          <div className="summary-row"><span>Version</span><span>{version || '—'}</span></div>
          <div className="summary-row"><span>License</span><span>GPLv3</span></div>
          <div className="summary-row"><span>Copyright</span><span>2026 archerprojects</span></div>
          <div className="summary-row"><span>Stack</span><span>Tauri 2 · Rust · React 18</span></div>
          <div className="summary-row"><span>Target</span><span>Linux x86_64</span></div>
          <div className="summary-row"><span>Developer</span><span>Developed by archerprojects</span></div>
          <div className="summary-row"><span>Contact</span><span>archer.projects@proton.me</span></div>
        </div>
      </div>

      <div className="card">
        <div className="section-label">What it does</div>
        <p style={{ fontSize: 13, lineHeight: 1.6, color: 'var(--text-primary)', marginTop: 8 }}>
          Boot OS Pro writes structured multiboot USB drives that boot via GRUB2 on EFI
          and legacy BIOS. Each persistent ISO gets its own ext4 partition with the OS
          squashfs extracted directly — fully writable, changes survive reboot, no
          distro-specific overlay. Live session ISOs are copied to a shared FAT32 free
          space partition and loopback-booted by GRUB.
        </p>
      </div>

      <div className="card">
        <div className="section-label">Supported distro families</div>
        <div className="summary-card" style={{ marginTop: 8 }}>
          <div className="summary-row"><span>Ubuntu / Mint / Debian Live</span><span>Persistent + Live</span></div>
          <div className="summary-row"><span>Fedora / RHEL / Rocky / Alma</span><span>Persistent + Live</span></div>
          <div className="summary-row"><span>Arch / Manjaro</span><span>Persistent + Live</span></div>
          <div className="summary-row"><span>Sparky / MX / antiX</span><span>Persistent + Live</span></div>
        </div>
      </div>

      <div className="card">
        <div className="section-label">Drive layout</div>
        <p style={{ fontSize: 12, fontFamily: 'var(--font-mono)', lineHeight: 1.8, color: 'var(--text-secondary)', marginTop: 8 }}>
          BIOS Boot (1 MiB) · EFI/ESP (512 MiB FAT32)<br/>
          BOOTOSPRO installer (40 MiB FAT32)<br/>
          ISO partitions (ext4, one per persistent OS)<br/>
          FREESPACE (FAT32, live session ISOs)
        </p>
      </div>

      <div className="card">
        <div className="section-label">Known limitations in this release</div>
        <div className="summary-card" style={{ marginTop: 8 }}>
          <div className="summary-row"><span>ARM (aarch64) boot</span><span style={{ color: 'var(--warning)' }}>Not supported — warning shown on add</span></div>
          <div className="summary-row"><span>Per-ISO live removal</span><span style={{ color: 'var(--warning)' }}>Clear all at once only</span></div>
          <div className="summary-row"><span>Manual kernel/initrd override</span><span style={{ color: 'var(--warning)' }}>Auto-detect only</span></div>
          <div className="summary-row"><span>BOOTOSPRO installer payload</span><span style={{ color: 'var(--warning)' }}>Partition created, not yet populated</span></div>
        </div>
      </div>
    </div>
  );
}

export default function App() {
  const [mode, setMode]     = useState<AppMode>('create');
  const [state, setState]   = useState<AppState>(INITIAL_STATE);
  const [report, setReport] = useState<DriveReport | null>(null);
  const [version, setVersion] = useState('');

  useEffect(() => { getVersion().then(setVersion).catch(() => setVersion('')); }, []);

  // Theme detection — sole owner. Polls every 30s for runtime switching.
  // index.html carries data-theme="dark" as pre-React default to prevent flash.
  useEffect(() => {
    const detect = () =>
      invoke<{ dark: boolean }>('get_theme_colors')
        .then(t => applyTheme(t.dark))
        .catch(() => applyTheme(true));

    detect();
    const timer = setInterval(detect, 30_000);
    return () => clearInterval(timer);
  }, []);

  const update = (patch: Partial<AppState>) => setState(s => ({ ...s, ...patch }));
  const goTo   = (step: Step) => update({ step });

  const syncFreeSpace = (isos: typeof state.isos, device: typeof state.device) => {
    const driveGb   = (device?.size_bytes ?? 0) / 1e9;
    const efiGb     = 0.5;
    const persistGb = isos
      .filter(i => i.iso_type === 'persistent')
      .reduce((s, i) => s + i.size_gb, 0);
    return Math.floor(Math.max(0, driveGb - efiGb - persistGb));
  };

  const handleIsosChange = (isos: typeof state.isos) => {
    update({ isos, free_space_gb: syncFreeSpace(isos, state.device) });
  };

  const handleDeviceSelect = (device: typeof state.device) => {
    update({ device, free_space_gb: syncFreeSpace(state.isos, device), isos: [] });
    setReport(null);
    if (device) {
      readDriveManifest(device.path)
        .then(setReport)
        .catch(() => setReport({ manifest: null, drift: false }));
    }
  };

  const resetAll = () => { setState(INITIAL_STATE); setReport(null); };

  // Drift resolution: rebuild the manifest from physical reality, then
  // re-read it so the UI reflects truth. Continue stays disabled until the
  // fresh report comes back drift-free.
  const handleReconcile = async () => {
    if (!state.device) return;
    const label = report?.manifest?.drive_label || 'BOOTOSPRO';
    await reconcileManifest(state.device.path, label);
    setReport(await readDriveManifest(state.device.path));
  };

  const isExisting = !!report?.manifest;

  return (
    <div className="app">
      <header className="topbar">
        <div className="logo">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 12h4M14 10h4M14 14h4"/>
          </svg>
        </div>
        <div className="app-identity">
          <span className="app-name">Boot OS Pro</span>
          <span className="app-sub">Multiboot USB Creator{version ? ` · v${version}` : ''}</span>
        </div>
        <div className="mode-tabs">
          <button className={`mode-tab ${mode === 'create' ? 'active' : ''}`} onClick={() => setMode('create')}>Select Drive</button>
          <button className={`mode-tab ${mode === 'manage' ? 'active' : ''}`} onClick={() => setMode('manage')}>Disk Manager</button>
          <button className={`mode-tab ${mode === 'about'  ? 'active' : ''}`} onClick={() => setMode('about')}>About</button>
        </div>
        {mode === 'create' && <StepBar current={state.step} />}
      </header>

      <main className="content">
        {mode === 'manage' ? (
          <DiskManager />
        ) : mode === 'about' ? (
          <AboutPanel version={version} />
        ) : (
          <>
            {state.step === 'configure' && (
              <ImagesStep
                device={state.device}
                isos={state.isos}
                freeSpaceGb={state.free_space_gb}
                report={report}
                isExisting={isExisting}
                onDeviceSelect={handleDeviceSelect}
                onChange={handleIsosChange}
                onReconcile={handleReconcile}
                onNext={() => goTo('summary')}
              />
            )}
            {state.step === 'summary' && (
              <SummaryStep
                state={state}
                report={report}
                isExisting={isExisting}
                onBack={() => goTo('configure')}
                onNext={() => goTo('write')}
              />
            )}
            {state.step === 'write' && (
              <WriteStep
                state={state}
                isExisting={isExisting}
                onDone={resetAll}
              />
            )}
          </>
        )}
      </main>
    </div>
  );
}
