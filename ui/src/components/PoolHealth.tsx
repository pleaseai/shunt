import type { ReactElement } from 'react';

import { pctReset, titleCase } from '../format';
import type { PoolAccount, PoolProvider } from '../types';
import type { Loadable } from '../useDashboard';

function poolState(account: PoolAccount): string {
  // `needs_relogin` is checked before the cooldown states on purpose: a dead
  // credential is *also* cooling down, and reporting only "cooling" is what made
  // a permanently dead account indistinguishable from a quota pause.
  if (account.disabled) return 'disabled';
  if (account.needs_relogin) return 'needs re-login';
  if (!account.has_state) return 'unseen';
  // The cooldown is checked before `near_quota`, matching `managedState` in
  // `accounts.ts` so the two tables cannot report the same account differently:
  // an account-wide cooldown is the fact that *all* of this account's traffic is
  // gated right now, while `near_quota` is a threshold warning about what is
  // coming. Reporting the warning while hiding the active gate is the defect.
  if (account.cooldown_secs_remaining) return 'cooling';
  if (account.near_quota) return 'near quota';
  if (account.cooldown_fable_secs_remaining) return 'cooling (fable)';
  return 'available';
}

function resetTitle(reset: number | null | undefined): string | undefined {
  return reset ? `resets ${new Date(reset * 1000).toLocaleString()}` : undefined;
}

function cooldownText(account: PoolAccount): string {
  return (
    [
      account.cooldown_secs_remaining ? `${account.cooldown_secs_remaining}s` : null,
      account.cooldown_fable_secs_remaining ? `${account.cooldown_fable_secs_remaining}s (fable)` : null,
    ]
      .filter(Boolean)
      .join(' · ') || '—'
  );
}

/** The shunt-owned credential lane, read-only. */
export function PoolHealth({ pool }: { pool: Loadable<PoolProvider[]> }): ReactElement {
  const rows =
    pool.status === 'ready'
      ? pool.data.flatMap((table) =>
          (table.accounts ?? []).map((account) => ({ provider: table.provider, account })),
        )
      : [];
  return (
    <>
      <h2>Managed pool health</h2>
      <div className="card overflow">
        <table>
          <thead>
            <tr>
              <th>Provider</th>
              <th>Account</th>
              <th>Plan</th>
              <th>State</th>
              <th>5h</th>
              <th>7d</th>
              <th>7d_oi</th>
              <th>Status</th>
              <th>Cooldown</th>
            </tr>
          </thead>
          <tbody id="pool">
            {pool.status === 'loading' ? (
              <tr>
                <td colSpan={9} className="muted">
                  Loading…
                </td>
              </tr>
            ) : null}
            {pool.status === 'error' ? (
              <tr>
                <td colSpan={9}>{pool.message}</td>
              </tr>
            ) : null}
            {pool.status === 'ready' && !rows.length ? (
              <tr>
                <td colSpan={9} className="muted">
                  No pooled accounts configured
                </td>
              </tr>
            ) : null}
            {rows.map(({ provider, account }) => {
              const state = poolState(account);
              return (
                <tr key={`${provider}:${account.name}`}>
                  <td>{provider}</td>
                  <td>{account.name}</td>
                  <td>{titleCase(account.plan) || '—'}</td>
                  <td
                    className={account.needs_relogin ? 'status' : undefined}
                    data-state={account.needs_relogin ? 'needs-relogin' : undefined}
                    title={
                      account.needs_relogin
                        ? 'The stored credential was permanently rejected, or cannot be refreshed. Re-add this account to sign in again.'
                        : undefined
                    }
                  >
                    {state}
                  </td>
                  <td title={resetTitle(account.reset_5h)}>
                    {pctReset(account.utilization_5h, account.reset_5h)}
                  </td>
                  <td title={resetTitle(account.reset_7d)}>
                    {pctReset(account.utilization_7d, account.reset_7d)}
                  </td>
                  <td title={resetTitle(account.reset_7d_oi)}>
                    {pctReset(account.utilization_7d_oi, account.reset_7d_oi)}
                  </td>
                  <td>{account.status || '—'}</td>
                  <td>{cooldownText(account)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}
