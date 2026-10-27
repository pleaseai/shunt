import { useEffect, useState, type ReactElement } from 'react';

import type { UsagePoll } from '../types';

/** `65` → `1:05`; whole minutes and seconds, no hours (the poller runs at least hourly). */
function minutesSeconds(totalSeconds: number): string {
  const seconds = Math.max(0, Math.round(totalSeconds));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

function ago(updatedAt: number, now: number): string {
  const seconds = Math.max(0, Math.round((now - updatedAt) / 1000));
  return seconds < 60 ? `${seconds}s ago` : `${Math.floor(seconds / 60)}m ago`;
}

export interface RefreshBarProps {
  /** When the usage table last finished loading (epoch ms). */
  updatedAt: number | null;
  /** The usage poller's cadence, or `null` when it is off or has not run. */
  usagePoll: UsagePoll | null;
  onRefresh: () => void;
}

/**
 * How fresh the usage numbers are, and when shunt next asks the providers for
 * them. Two clocks, deliberately kept apart:
 * - the page re-reads shunt's own pool state on a timer (`AUTO_REFRESH_MS`);
 * - the usage poller asks each provider's usage API every
 *   `usage_refresh_seconds`.
 *
 * Between polls, the account serving traffic still updates on every response.
 * Reload repeats the page's own load, local logins included, and never runs
 * the usage poll early. It is not the per-account Refresh, which renews that
 * account's credentials.
 */
export function RefreshBar({ updatedAt, usagePoll, onRefresh }: RefreshBarProps): ReactElement {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const tick = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(tick);
  }, []);

  const nextIn = usagePoll ? usagePoll.nextAt - now / 1000 : null;
  return (
    <div className="refresh-bar muted" data-refresh-bar>
      {updatedAt !== null ? <span>Updated {ago(updatedAt, now)}</span> : null}
      {nextIn !== null ? (
        <span
          title={`shunt asks each provider for every account's usage every ${usagePoll?.intervalSeconds}s; the account serving traffic also updates on every response`}
        >
          {nextIn > 0 ? `Next quota check in ${minutesSeconds(nextIn)}` : 'Quota check due'}
        </span>
      ) : null}
      <button
        type="button"
        className="secondary"
        onClick={onRefresh}
        title="Re-read this page's data now. Does not ask the providers for usage early."
      >
        Reload
      </button>
    </div>
  );
}
