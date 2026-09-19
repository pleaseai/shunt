import { Collapsible } from '@base-ui/react/collapsible';
import { useCallback, useRef, type ReactElement } from 'react';

import { AddClaudeAccount, type AddAccountHandle } from './components/AddClaudeAccount';
import { AddCodexAccount } from './components/AddCodexAccount';
import { AddAntigravityAccount } from './components/AddAntigravityAccount';
import { AntigravityAccounts } from './components/AntigravityAccounts';
import { ClaudeAccounts } from './components/ClaudeAccounts';
import { CodexAccounts } from './components/CodexAccounts';
import { ObservedAccounts } from './components/ObservedAccounts';
import { PoolHealth } from './components/PoolHealth';
import { UpstreamStatus } from './components/UpstreamStatus';
import { useCanWrite } from './session';
import { useDashboard } from './useDashboard';

export function Dashboard(): ReactElement {
  const data = useDashboard();
  const canWrite = useCanWrite();
  const claudeForm = useRef<AddAccountHandle>(null);
  const codexForm = useRef<AddAccountHandle>(null);
  const antigravityForm = useRef<AddAccountHandle>(null);

  const {
    reloadObserved,
    reloadAccounts,
    reloadCodexAccounts,
    reloadAntigravityAccounts,
    reloadPool,
  } = data;

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

  const afterAntigravityMutation = useCallback(() => {
    void reloadObserved();
    void reloadAntigravityAccounts();
    void reloadPool();
  }, [reloadObserved, reloadAntigravityAccounts, reloadPool]);

  return (
    <>
      <UpstreamStatus sources={data.status} />

      {/* Usage first: it is what an operator opens this page for. Pool
          management is a rarer, riskier task, so it sits behind a disclosure
          rather than above the numbers. */}
      <ObservedAccounts observed={data.observed} />

      <Collapsible.Root className="mt-8" data-pool-management>
        <Collapsible.Trigger className="collapsible-trigger secondary w-full justify-start text-left text-text-secondary">
          <strong className="text-text">Manage pool accounts</strong>{' '}
          <span className="muted">(advanced)</span>
        </Collapsible.Trigger>
        <Collapsible.Panel className="pt-1" hiddenUntilFound>
        <p className="muted">
          Managed accounts are separate credential copies owned and refreshed by shunt for
          load-balancing. You do not need them merely to view usage.
        </p>

        {/* A read session keeps every table below — it may read all of them —
            and loses only what it cannot do. Saying so is the point: a section
            that simply lost its buttons reads as a broken page. */}
        {canWrite ? (
          <>
            <AddClaudeAccount ref={claudeForm} onStored={afterClaudeMutation} />
            <AddCodexAccount ref={codexForm} onStored={afterCodexMutation} />
            <AddAntigravityAccount ref={antigravityForm} onStored={afterAntigravityMutation} />
          </>
        ) : (
          <p className="msg">
            This is a read-only admin session, so adding, re-authenticating, and removing
            accounts are not available. Sign in with a write-tier admin key to manage the pool.
          </p>
        )}

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
        <AntigravityAccounts
          accounts={data.antigravityAccounts}
          onRelogin={(name) => antigravityForm.current?.prime(name)}
          onMutated={afterAntigravityMutation}
          onMessage={(text, ok) => antigravityForm.current?.report(text, ok)}
        />
        <PoolHealth pool={data.pool} />
        </Collapsible.Panel>
      </Collapsible.Root>
    </>
  );
}
