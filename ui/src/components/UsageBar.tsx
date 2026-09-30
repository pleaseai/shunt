import type { ReactElement } from 'react';

import { untilShort } from '../format';

/**
 * One quota window as a labelled bar. The three `aria-value*` attributes are
 * what make the number reachable without sight of the fill; the visible caption
 * repeats it for everyone else.
 *
 * A native `<progress>` rather than a nested div whose fill width is set from
 * script: `value`/`max` carry the proportion declaratively, and the element
 * brings the progressbar role with it. The gradient fill and its over-quota
 * variant are reproduced on `::-webkit-progress-value` / `::-moz-progress-bar`
 * in `index.css`, so the bar looks as it did.
 *
 * The `aria-value*` trio is kept even though `<progress>` implies the same
 * semantics: the implicit values are computed by the accessibility layer, so
 * nothing reading attributes can see them -- which is both how this surface is
 * tested and what a DOM-scraping assistive tool sees.
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
      <progress
        className="usage-track"
        aria-label={`${label} usage`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={used}
        max={100}
        value={used}
        data-level={used >= 100 ? 'full' : undefined}
      />
    </div>
  );
}
