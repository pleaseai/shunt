import type { ReactElement } from 'react';

import { accountStatus } from '../accounts';
import { API, mutate } from '../api';
import { when } from '../format';
import { useCanWrite, useSession } from '../session';
import type { ClaudeStoreAccount } from '../types';
import type { Loadable } from '../useDashboard';

export interface ClaudeAccountsProps {
  accounts: Loadable<ClaudeStoreAccount[]>;
  /** Prime the add form with this account's name and login method. */
  onRelogin: (name: string, kind: string) => void;
  /** Re-read every table a store mutation can have changed. */
  onMutated: () => void;
  /** Report into the add form's message area, where this page's writes speak. */
  onMessage: (text: string, ok: boolean) => void;
}

export function ClaudeAccounts({
  accounts,
  onRelogin,
  onMutated,
  onMessage,
}: ClaudeAccountsProps): ReactElement {
  const { csrf, expiryBufferMs } = useSession();
  const canWrite = useCanWrite();
  // The actions column goes away entirely for a read session rather than
  // standing empty, so the placeholder spans have to follow it.
  const columns = canWrite ? 5 : 4;

  async function remove(name: string): Promise<void> {
    if (!window.confirm(`Remove account '${name}'? This deletes its stored token file.`)) return;
    try {
      const result = await mutate(`${API}/accounts/claude/${encodeURIComponent(name)}`, csrf, {
        method: 'DELETE',
      });
      if (!result.ok) {
        onMessage(result.message ?? 'Failed to remove', false);
        return;
      }
      onMutated();
    } catch {
      onMessage('Request failed', false);
    }
  }

  async function refresh(name: string): Promise<void> {
    try {
      const result = await mutate(`${API}/accounts/claude/${encodeURIComponent(name)}/refresh`, csrf, {
        method: 'POST',
      });
      // The tables are re-read on both paths: the probe can set *or* clear
      // `needs_relogin`, and the primary Accounts table renders it, so skipping
      // the failure path would leave the top table showing the pre-probe state
      // until a page reload.
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
      <h2>Claude accounts</h2>
      <div className="card overflow">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Status</th>
              <th>UUID</th>
              {canWrite ? <th /> : null}
            </tr>
          </thead>
          <tbody id="accounts">
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
                  No store accounts yet
                </td>
              </tr>
            ) : null}
            {accounts.status === 'ready'
              ? accounts.data.map((account) => {
                  const info = accountStatus(account.kind, account.expires_at, expiryBufferMs);
                  return (
                    <tr key={account.name}>
                      <td>{account.name}</td>
                      <td>{account.kind}</td>
                      <td
                        className="status"
                        data-state={info.state}
                        title={
                          account.expires_at
                            ? `access token expires ${when(account.expires_at)}`
                            : undefined
                        }
                      >
                        {info.text}
                        <small className="status-note">{info.note}</small>
                      </td>
                      <td className="mono">{account.uuid || '—'}</td>
                      {canWrite ? (
                        <td className="row-actions">
                          {/* Only an imported login carries a refresh grant; a
                              setup-token account has nothing to probe (the
                              endpoint refuses it), so it gets no button. */}
                          {account.kind === 'imported' ? (
                            <button
                              type="button"
                              className="secondary compact"
                              title="Exercise this account's refresh grant now and report whether the login is still alive"
                              onClick={() => void refresh(account.name)}
                            >
                              Refresh
                            </button>
                          ) : null}
                          <button
                            type="button"
                            className="secondary compact"
                            onClick={() => onRelogin(account.name, account.kind)}
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
                  );
                })
              : null}
          </tbody>
        </table>
      </div>
    </>
  );
}
