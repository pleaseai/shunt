import type { ReactElement } from 'react';

import { API, mutate } from '../api';
import { when } from '../format';
import { useCanWrite, useSession } from '../session';
import type { AntigravityStoreAccount } from '../types';
import type { Loadable } from '../useDashboard';

export interface AntigravityAccountsProps {
  accounts: Loadable<AntigravityStoreAccount[]>;
  onRelogin: (name: string) => void;
  onMutated: () => void;
  onMessage: (text: string, ok: boolean) => void;
}

/**
 * Every Antigravity store account is refreshable — `store_oauth_tokens`
 * (`auth/antigravity/store.rs`) rejects a blank refresh token — so, like the
 * Codex table, the status column is unconditional. Unlike Codex, a real
 * refresh probe exists (`AntigravityAuthStore::force_refresh_if_access_token`),
 * so this table gets the Refresh button Codex's does not.
 */
export function AntigravityAccounts({
  accounts,
  onRelogin,
  onMutated,
  onMessage,
}: AntigravityAccountsProps): ReactElement {
  const { csrf } = useSession();
  const canWrite = useCanWrite();
  const columns = canWrite ? 4 : 3;

  async function remove(name: string): Promise<void> {
    if (!window.confirm(`Remove Antigravity account '${name}'? This deletes its stored token file.`))
      return;
    try {
      const result = await mutate(`${API}/accounts/antigravity/${encodeURIComponent(name)}`, csrf, {
        method: 'DELETE',
      });
      if (!result.ok) {
        onMessage(result.message ?? 'Failed to remove Antigravity account', false);
        return;
      }
      onMutated();
    } catch {
      onMessage('Request failed', false);
    }
  }

  async function refresh(name: string): Promise<void> {
    try {
      const result = await mutate(
        `${API}/accounts/antigravity/${encodeURIComponent(name)}/refresh`,
        csrf,
        { method: 'POST' },
      );
      // Re-read on both paths: the probe can set *or* clear `needs_relogin`,
      // which the primary Accounts table renders.
      if (!result.ok) {
        onMessage(result.message ?? 'Refresh failed', false);
        onMutated();
        return;
      }
      onMessage(
        (result.payload.message as string | undefined) ?? 'Refresh succeeded',
        result.payload.needs_relogin !== true,
      );
      onMutated();
    } catch {
      onMessage('Request failed', false);
    }
  }

  return (
    <>
      <h2>Antigravity accounts</h2>
      <div className="card overflow">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Status</th>
              <th>Email</th>
              {canWrite ? <th /> : null}
            </tr>
          </thead>
          <tbody id="antigravity-accounts">
            {accounts.status === 'loading' ? (
              <tr>
                <td colSpan={columns} className="muted">
                  Loading…
                </td>
              </tr>
            ) : null}
            {accounts.status === 'error' ? (
              <tr>
                <td colSpan={columns}>{accounts.message}</td>
              </tr>
            ) : null}
            {accounts.status === 'ready' && !accounts.data.length ? (
              <tr>
                <td colSpan={columns} className="muted">
                  No Antigravity store accounts yet
                </td>
              </tr>
            ) : null}
            {accounts.status === 'ready'
              ? accounts.data.map((account) => (
                  <tr key={account.name}>
                    <td>{account.name}</td>
                    <td
                      className="status"
                      data-state="available"
                      title={
                        account.expires_at
                          ? `access token expires ${when(account.expires_at)}`
                          : undefined
                      }
                    >
                      Auto-refreshes
                      <small className="status-note">shunt renews this login as needed</small>
                    </td>
                    <td>{account.email || '—'}</td>
                    {canWrite ? (
                      <td className="row-actions">
                        <button
                          type="button"
                          className="secondary compact"
                          title="Exercise this account's refresh grant now and report whether the login is still alive"
                          onClick={() => void refresh(account.name)}
                        >
                          Refresh
                        </button>
                        <button
                          type="button"
                          className="secondary compact"
                          onClick={() => onRelogin(account.name)}
                        >
                          Re-login
                        </button>
                        <button
                          type="button"
                          className="danger"
                          onClick={() => void remove(account.name)}
                        >
                          Remove
                        </button>
                      </td>
                    ) : null}
                  </tr>
                ))
              : null}
          </tbody>
        </table>
      </div>
    </>
  );
}
