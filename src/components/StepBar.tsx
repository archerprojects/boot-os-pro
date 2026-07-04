import type { Step } from '../lib/types';

const STEPS: { id: Step; label: string }[] = [
  { id: 'configure', label: 'Configure' },
  { id: 'summary',   label: 'Summary' },
  { id: 'write',     label: 'Write' },
];

interface Props { current: Step; }

export function StepBar({ current }: Props) {
  const currentIdx = STEPS.findIndex(s => s.id === current);
  return (
    <nav className="stepbar" aria-label="Progress">
      {STEPS.map((step, i) => {
        const done   = i < currentIdx;
        const active = i === currentIdx;
        return (
          <div
            key={step.id}
            className={`step-pill ${active ? 'active' : ''} ${done ? 'done' : ''}`}
            aria-current={active ? 'step' : undefined}
          >
            <span className="step-num">{done ? '✓' : i + 1}</span>
            <span className="step-label">{step.label}</span>
          </div>
        );
      })}
    </nav>
  );
}
