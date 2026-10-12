import type { CSSProperties, ReactElement } from 'react';

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
 *
 * `cap` is the account's configured `max_utilization` hard cap for this window
 * as a fraction. It is drawn as a decorative tick over the bar and named in the
 * caption. `atCap` is the caller's verdict that selection excludes the account
 * at this window, and makes the bar read as full. The bar does not derive it
 * from `remaining`: the value it displays may be a client observation rather
 * than the utilization the pool's cap check reads.
 *
 * The tick is positioned through a `--cap` custom property set by React's
 * `style` prop, which the client renderer applies through the CSSOM
 * (`style.setProperty`) rather than as a `style="..."` attribute, so it is
 * allowed under the shell's `style-src 'self'` -- which has no
 * `'unsafe-inline'` (`src/admin/ui.rs`). The rule that consumes it lives in
 * `index.css`.
 */
export function UsageBar({
  label,
  remaining,
  resetTime,
  cap,
  atCap = false,
}: {
  label: string;
  remaining: number;
  resetTime: string | null;
  cap?: number | null;
  atCap?: boolean;
}): ReactElement {
  const used = Math.max(0, Math.min(100, Math.round((1 - remaining) * 1000) / 10));
  const capPct = cap === null || cap === undefined ? null : Math.round(cap * 1000) / 10;
  return (
    <div className="usage-item">
      <div className="usage-meta">
        <span>{label}</span>
        <span className="usage-value">
          {used}% used
          {capPct === null ? '' : ` · cap ${capPct}%`}
          {resetTime ? ` · ${untilShort(Date.parse(resetTime) / 1000)}` : ''}
        </span>
      </div>
      <div className="usage-bar">
        <progress
          className="usage-track"
          aria-label={`${label} usage`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={used}
          max={100}
          value={used}
          data-level={used >= 100 || atCap ? 'full' : undefined}
        />
        {capPct === null ? null : (
          <span
            className="usage-cap"
            aria-hidden="true"
            style={{ '--cap': `${Math.max(0, Math.min(100, capPct))}%` } as CSSProperties}
          />
        )}
      </div>
    </div>
  );
}
