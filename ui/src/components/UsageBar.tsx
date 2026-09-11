import type { ReactElement } from 'react';

import { untilShort } from '../format';

/**
 * One quota window as a labelled bar. `role="progressbar"` with the three
 * `aria-value*` attributes is what makes the number reachable without sight of
 * the fill; the visible caption repeats it for everyone else.
 */
export function UsageBar({
  label,
  remaining,
  resetTime,
}: {
  label: string;
  remaining: number;
  resetTime: string | null;
}): ReactElement {
  const used = Math.max(0, Math.min(100, Math.round((1 - remaining) * 1000) / 10));
  return (
    <div className="usage-item">
      <div className="usage-meta">
        <span>{label}</span>
        <span className="usage-value">
          {used}% used
          {resetTime ? ` · ${untilShort(Date.parse(resetTime) / 1000)}` : ''}
        </span>
      </div>
      <div
        className="usage-track"
        role="progressbar"
        aria-label={`${label} usage`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={used}
      >
        <div
          className="usage-fill"
          style={{ width: `${used}%` }}
          data-level={used >= 100 ? 'full' : undefined}
        />
      </div>
    </div>
  );
}
