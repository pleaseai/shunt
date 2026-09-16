import type { ReactElement } from 'react';

import { API, mutate } from '../api';
import { when } from '../format';
import { useCanWrite, useSession } from '../session';
import type { CodexStoreAccount } from '../types';
import type { Loadable } from '../useDashboard';

export interface CodexAccountsProps {
  accounts: Loadable<CodexStoreAccount[]>;
  onRelogin: (name: string) => void;
  onMutated: () => void;
  onMessage: (text: string, ok: boolean) => void;
}

/**
 * Unlike the Claude table's kind-derived status, this column is unconditional:
 * every Codex store account is refreshable. Both writers into the store reject a
 * missing or empty refresh token (`import_auth` and `store_chatgpt_tokens` in
 * `auth/codex/store.rs`) and there is no setup-token analog, so shunt owns
 * renewal for all of them. That expiry is therefore never the operator's
 * problem — printing it raw, as this column used to, made every healthy account
 * read as broken within the hour. It is kept as the tooltip.
 */
export function CodexAccounts({
  accounts,
  onRelogin,
  onMutated,
  onMessage,
}: CodexAccountsProps): ReactElement {
  const { csrf } = useSession();
  const canWrite = useCanWrite();
  const columns = canWrite ? 4 : 3;

  async function remove(name: string): Promise<void> {
    if (!window.confirm(`Remove Codex account '${name}'? This deletes its stored token file.`)) return;
    try {
      const result = await mutate(`${API}/accounts/codex/${encodeURIComponent(name)}`, csrf, {
        method: 'DELETE',
      });
      if (!result.ok) {
        onMessage(result.message ?? 'Failed to remove Codex account', false);
        return;
      }
      onMutated();
    } catch {
      onMessage('Request failed', false);
    }
  }

  return (
    <>
      <h2>Codex accounts</h2>
      <div className="card overflow">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Status</th>
              <th>Account ID</th>
              {canWrite ? <th /> : null}
            </tr>
          </thead>
          <tbody id="codex-accounts">
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
                  No Codex store accounts yet
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
                    <td className="mono">{account.account_id || '—'}</td>
                    {canWrite ? (
                      <td className="row-actions">
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
