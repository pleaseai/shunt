import type { ReactElement, ReactNode } from 'react';

import { effectiveState, rowStatusText } from '../accounts';
import { untilShort } from '../format';
import { ProviderName, providerLabel } from '../providers';
import type { AccountRow, QuotaBucket } from '../types';
import type { Loadable } from '../useDashboard';
import { UsageBar } from './UsageBar';

/** The one-line remediation shown under a row's status, when there is one. */
function statusNote(row: AccountRow, state: string): string | null {
  switch (state) {
    case 'waiting-for-traffic':
      return 'Quota arrives in GPT response headers';
    case 'expired':
      return 'The provider client owns refresh';
    case 'unavailable':
      return 'Current login could not read quota';
    case 'needs-relogin':
      return 'Re-add this account to sign in again';
    default:
      break;
  }
  const now = Math.floor(Date.now() / 1000);
  // Both cooldowns can run at once, and each names a different window. Testing
  // them in sequence and returning on the first made the Fable deadline
  // unreachable whenever the account-wide one was set; they are joined here the
  // way the pool table's `cooldownText` already joins them.
  const notes = [
    row.managed?.cooldown_secs_remaining
      ? `retries in ${untilShort(now + row.managed.cooldown_secs_remaining)}`
      : null,
    row.managed?.cooldown_fable_secs_remaining
      ? `Fable retries in ${untilShort(now + row.managed.cooldown_fable_secs_remaining)}`
      : null,
  ].filter(Boolean);
  return notes.length ? notes.join(' · ') : null;
}

function identityTitle(row: AccountRow): string | undefined {
  if (row.managed && row.observed) {
    return `managed pool account · same subscription as the local ${providerLabel(row.provider)} login`;
  }
  if (row.managed) return 'managed pool account';
  if (row.observed) return `${row.observed.source} · read-only`;
  return undefined;
}

function bucketsTitle(buckets: QuotaBucket[]): string {
  return buckets
    .map((bucket) => {
      const used = Math.round((1 - (bucket.remaining ?? 0)) * 1000) / 10;
      const resets = bucket.reset_time ? `, resets ${new Date(bucket.reset_time).toLocaleString()}` : '';
      return `${bucket.label}: ${used}% used${resets}`;
    })
    .join('\n');
}

function emptyUsageText(state: string): string {
  if (state === 'expired') return 'Sign in again with the provider client';
  if (state === 'waiting-for-traffic') return 'Send one GPT request through this shunt';
  if (state === 'connected') return 'Usage integration in progress';
  return 'No usage reported yet';
}

function UsageCell({ row, state }: { row: AccountRow; state: string }): ReactElement {
  const buckets = (row.observed?.quota_buckets ?? []).filter(
    (bucket) => bucket.remaining !== null && bucket.remaining !== undefined,
  );
  if (buckets.length) {
    return (
      <td className="usage-lines" title={bucketsTitle(buckets)}>
        {buckets.map((bucket) => (
          <UsageBar
            key={bucket.label}
            label={bucket.label}
            remaining={bucket.remaining as number}
            resetTime={bucket.reset_time}
          />
        ))}
      </td>
    );
  }
  const windows: [string, number | null | undefined, number | null | undefined][] = [
    ['5h', row.utilization_5h, row.reset_5h],
    ['Week', row.utilization_7d, row.reset_7d],
    ['Fable', row.utilization_7d_oi, row.reset_7d_oi],
  ];
  const present = windows.filter(([, value]) => value !== null && value !== undefined);
  return (
    <td className="usage-lines">
      {present.length ? (
        present.map(([label, value, reset]) => (
          <UsageBar
            key={label}
            label={label}
            remaining={1 - (value as number)}
            resetTime={reset ? new Date(reset * 1000).toISOString() : null}
          />
        ))
      ) : (
        <span className="usage-empty">{emptyUsageText(state)}</span>
      )}
    </td>
  );
}

function Row({ row, first }: { row: AccountRow; first: boolean }): ReactElement {
  // The label, the `data-state` used for styling, and the remediation note all
  // read one derivation, so they cannot disagree — a "Needs login" label beside
  // a green "available" dot with no login hint is exactly what three separate
  // derivations produced.
  const state = effectiveState(row);
  const note = statusNote(row, state);
  return (
    <tr className={state === 'connected' ? 'pending-row' : undefined}>
      {/* The provider is named once per group; continuation rows keep the grid
          column so the account labels stay aligned under it. */}
      {first ? (
        <td>
          <ProviderName provider={row.provider} />
        </td>
      ) : (
        <td className="provider-continued" />
      )}
      <td title={identityTitle(row)}>
        {row.label}
        {row.detail ? <small className="account-detail">{row.detail}</small> : null}
      </td>
      <td className="status" data-state={state} title={row.observed?.message ?? undefined}>
        {rowStatusText(state)}
        {note ? <small className="status-note">{note}</small> : null}
      </td>
      <UsageCell row={row} state={state} />
    </tr>
  );
}

function Message({ children, muted }: { children: ReactNode; muted?: boolean }): ReactElement {
  return (
    <tr>
      <td colSpan={4} className={muted ? 'muted' : undefined}>
        {children}
      </td>
    </tr>
  );
}

/**
 * The primary table: managed pool accounts and read-only local observations,
 * folded together so one subscription seen through two lenses reads as one
 * account.
 */
export function ObservedAccounts({
  observed,
}: {
  observed: Loadable<Map<string, AccountRow[]>>;
}): ReactElement {
  return (
    <>
      <h2>Accounts and usage</h2>
      <p className="muted">
        Read-only signals from provider clients on this machine.{' '}
        <strong>Waiting for traffic</strong> means GPT has not returned quota headers to this shunt
        yet; <strong>Needs login</strong> means the provider-owned access token expired and must be
        renewed by that provider client.
      </p>
      <div className="card overflow">
        <table id="observed-table">
          <thead>
            <tr>
              <th>Provider</th>
              <th>Account</th>
              <th>Status</th>
              <th>Usage</th>
            </tr>
          </thead>
          <tbody id="observed">
            {observed.status === 'loading' ? <Message muted>Loading…</Message> : null}
            {observed.status === 'error' ? <Message>{observed.message}</Message> : null}
            {observed.status === 'ready'
              ? (() => {
                  const groups = [...observed.data];
                  const total = groups.reduce((sum, [, rows]) => sum + rows.length, 0);
                  if (!total) {
                    return (
                      <Message muted>
                        No supported local provider login found. Sign in with a provider CLI.
                      </Message>
                    );
                  }
                  return groups.flatMap(([provider, rows]) =>
                    rows.map((row, index) => (
                      <Row key={`${provider}:${row.label}:${index}`} row={row} first={index === 0} />
                    )),
                  );
                })()
              : null}
          </tbody>
        </table>
      </div>
    </>
  );
}
