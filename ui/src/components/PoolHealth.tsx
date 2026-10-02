import { useState, type ReactElement } from 'react';

import { patchPoolAccount, patchPoolSortByReset } from '../api';
import { pctReset, titleCase } from '../format';
import { useCanWrite, useSession } from '../session';
import type { PoolAccount, PoolData } from '../types';
import type { Loadable } from '../useDashboard';

function poolState(account: PoolAccount): string {
  // `needs_relogin` is checked before the cooldown states on purpose: a dead
  // credential is *also* cooling down, and reporting only "cooling" is what made
  // a permanently dead account indistinguishable from a quota pause.
  if (account.disabled) return 'disabled';
  // A permanently dead credential remains the most actionable state even if
  // the operator has also paused this provider lane.
  if (account.needs_relogin) return 'needs re-login';
  if (account.paused) return 'paused';
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

export interface PoolHealthProps {
  pool: Loadable<PoolData>;
  onMutated: () => void;
}

/**
 * The shunt-owned credential lane. Read-only, except pausing an account and
 * toggling the reset-priority sort — both runtime overrides that take effect
 * without editing `shunt.toml`.
 */
export function PoolHealth({ pool, onMutated }: PoolHealthProps): ReactElement {
  const { csrf } = useSession();
  const canWrite = useCanWrite();
  const [message, setMessage] = useState<string | null>(null);
  const columns = canWrite ? 10 : 9;

  async function togglePause(provider: string, accountRef: string, paused: boolean): Promise<void> {
    try {
      const result = await patchPoolAccount(csrf, provider, accountRef, paused);
      if (!result.ok) {
        setMessage(result.message ?? `Failed to ${paused ? 'pause' : 'resume'} account`);
        return;
      }
      setMessage(null);
      onMutated();
    } catch {
      setMessage('Request failed');
    }
  }

  async function toggleSortByReset(sortByReset: boolean): Promise<void> {
    try {
      const result = await patchPoolSortByReset(csrf, sortByReset);
      if (!result.ok) {
        setMessage(result.message ?? 'Failed to change pool sort order');
        return;
      }
      setMessage(null);
      onMutated();
    } catch {
      setMessage('Request failed');
    }
  }

  const rows =
    pool.status === 'ready'
      ? pool.data.providers.flatMap((table) =>
          (table.accounts ?? []).map((account) => ({ provider: table.provider, account })),
        )
      : [];
  return (
    <>
      <h2>Managed pool health</h2>
      {canWrite && pool.status === 'ready' ? (
        <p className="muted">
          <label>
            <input
              type="checkbox"
              checked={pool.data.sortByReset}
              onChange={(event) => void toggleSortByReset(event.target.checked)}
            />{' '}
            Rank available accounts by soonest quota reset instead of burn-rate headroom
          </label>
        </p>
      ) : null}
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
              {canWrite ? <th /> : null}
            </tr>
          </thead>
          <tbody id="pool">
            {pool.status === 'loading' ? (
              <tr>
                <td colSpan={columns} className="muted">
                  Loading…
                </td>
              </tr>
            ) : null}
            {pool.status === 'error' ? (
              <tr>
                <td colSpan={columns}>{pool.message}</td>
              </tr>
            ) : null}
            {pool.status === 'ready' && !rows.length ? (
              <tr>
                <td colSpan={columns} className="muted">
                  No pooled accounts configured
                </td>
              </tr>
            ) : null}
            {rows.map(({ provider, account }, rowIndex) => {
              const state = poolState(account);
              return (
                <tr key={`${provider}:${account.account_ref ?? account.name}:${rowIndex}`}>
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
                  {canWrite ? (
                    <td className="row-actions">
                      <button
                        type="button"
                        className="secondary compact"
                        title={
                          account.paused
                            ? 'Resume selecting this account'
                            : 'Exclude this account from selection without touching config'
                        }
                        disabled={!account.account_ref}
                        onClick={() => {
                          if (account.account_ref) {
                            void togglePause(provider, account.account_ref, !account.paused);
                          }
                        }}
                      >
                        {account.paused ? 'Resume' : 'Pause'}
                      </button>
                    </td>
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {message ? <p className="msg">{message}</p> : null}
    </>
  );
}
