import type { ReactElement } from 'react';

import { when } from '../format';
import type { StatusSource } from '../types';

function statusLabel(indicator: string): string {
  const labels: Record<string, string> = {
    none: 'Operational',
    minor: 'Minor issues',
    major: 'Major outage',
    critical: 'Critical outage',
    unknown: 'Unknown',
  };
  return labels[indicator] ?? indicator;
}

/**
 * `[server.status]` is opt-in and observation-only, so the whole section is
 * absent rather than empty when nothing is configured — an operator should not
 * have to read a blank table to learn a feature is off.
 */
export function UpstreamStatus({ sources }: { sources: StatusSource[] | null }): ReactElement | null {
  if (!sources || !sources.length) return null;
  return (
    <div id="status-section">
      <h2>Upstream status</h2>
      <p className="muted">
        Provider-reported status from each configured Statuspage endpoint (<code>[server.status]</code>
        ). Observation only — never consulted by routing or failover.
      </p>
      <div className="card overflow">
        <table>
          <thead>
            <tr>
              <th>Provider</th>
              <th>Status</th>
              <th>Description</th>
              <th>Observed</th>
            </tr>
          </thead>
          <tbody id="status">
            {sources.map((source) => (
              <tr key={source.provider}>
                <td>{source.provider}</td>
                <td className="status" data-state={source.indicator}>
                  {statusLabel(source.indicator)}
                </td>
                {/* The visible text is already short (the server keeps error
                    details to a single line, ~160 chars); the title just makes
                    that same detail reachable on hover too. */}
                <td title={source.error ?? undefined}>{source.error || source.description || '—'}</td>
                <td>{when(source.observed_at ? source.observed_at * 1000 : null)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
