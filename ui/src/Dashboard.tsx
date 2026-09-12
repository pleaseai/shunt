import { useCallback, useRef, type ReactElement } from 'react';

import { API } from './api';
import { AddClaudeAccount, type AddAccountHandle } from './components/AddClaudeAccount';
import { AddCodexAccount } from './components/AddCodexAccount';
import { ClaudeAccounts } from './components/ClaudeAccounts';
import { CodexAccounts } from './components/CodexAccounts';
import { ObservedAccounts } from './components/ObservedAccounts';
import { PoolHealth } from './components/PoolHealth';
import { UpstreamStatus } from './components/UpstreamStatus';
import { useDashboard } from './useDashboard';

/**
 * Sign-out is a scripted POST rather than the server-rendered page's form: the
 * shell is served under `form-action 'none'` (`src/admin/ui.rs`), which is what
 * lets that policy stay tight for a bundle that posts no forms at all. The
 * endpoint answers `303` to `/admin/login` and clears the cookie on the way, and
 * `same_origin` — not a CSRF token — is what guards it, which a same-origin
 * `fetch` satisfies.
 */
function SignOut(): ReactElement {
  async function signOut(): Promise<void> {
    try {
      await fetch(`${API}/logout`, { method: 'POST' });
    } catch {
      // The cookie may or may not be cleared; the login page settles it.
    }
    window.location.assign('/admin/login');
  }
  return (
    <button className="secondary" type="button" onClick={() => void signOut()}>
      Sign out
    </button>
  );
}

export function Dashboard(): ReactElement {
  const data = useDashboard();
  const claudeForm = useRef<AddAccountHandle>(null);
  const codexForm = useRef<AddAccountHandle>(null);

  const { reloadObserved, reloadAccounts, reloadCodexAccounts, reloadPool } = data;

  // Every store mutation re-reads the grouped table too, not just the store
  // table it changed: that view renders `needs_relogin` and the coalesced
  // managed/observed state, both of which a mutation can change, so omitting it
  // would leave the primary table showing pre-mutation state until a reload.
  const afterClaudeMutation = useCallback(() => {
    void reloadObserved();
    void reloadAccounts();
    void reloadPool();
  }, [reloadObserved, reloadAccounts, reloadPool]);

  const afterCodexMutation = useCallback(() => {
    void reloadObserved();
    void reloadCodexAccounts();
    void reloadPool();
  }, [reloadObserved, reloadCodexAccounts, reloadPool]);

  return (
    <main>
      <header>
        <h1>shunt admin</h1>
        <SignOut />
      </header>

      <UpstreamStatus sources={data.status} />

      {/* Usage first: it is what an operator opens this page for. Pool
          management is a rarer, riskier task, so it sits behind a disclosure
          rather than above the numbers. */}
      <ObservedAccounts observed={data.observed} />

      <details style={{ marginTop: '2rem' }}>
        <summary>
          <strong>Manage pool accounts</strong> <span className="muted">(advanced)</span>
        </summary>
        <p className="muted">
          Managed accounts are separate credential copies owned and refreshed by shunt for
          load-balancing. You do not need them merely to view usage.
        </p>

        <AddClaudeAccount ref={claudeForm} onStored={afterClaudeMutation} />
        <AddCodexAccount ref={codexForm} onStored={afterCodexMutation} />

        <ClaudeAccounts
          accounts={data.accounts}
          onRelogin={(name, kind) => claudeForm.current?.prime(name, kind)}
          onMutated={afterClaudeMutation}
          onMessage={(text, ok) => claudeForm.current?.report(text, ok)}
        />
        <CodexAccounts
          accounts={data.codexAccounts}
          onRelogin={(name) => codexForm.current?.prime(name)}
          onMutated={afterCodexMutation}
          onMessage={(text, ok) => codexForm.current?.report(text, ok)}
        />
        <PoolHealth pool={data.pool} />
      </details>
    </main>
  );
}
