import { useCallback, useEffect, useRef, useState } from 'react';

import { accountGroups } from './accounts';
import { API, readJson } from './api';
import type {
  AccountRow,
  ClaudeStoreAccount,
  CodexStoreAccount,
  ObservedAccount,
  PoolProvider,
  StatusSource,
} from './types';

export type Loadable<T> =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'ready'; data: T };

/**
 * A read whose older response can never repaint over a newer one.
 *
 * Every mutation handler reloads several of these tables, so two loads of the
 * same endpoint are routinely in flight at once; without the counter the older
 * response lands last and wins, which after a Refresh means the table falls
 * back to the pre-probe state. The counter is bumped when a load starts and
 * checked when it settles, so a superseded load neither renders nor blanks what
 * the newer one drew.
 */
function useSequencedLoad<T>(
  load: () => Promise<Loadable<T>>,
): [Loadable<T>, () => Promise<void>] {
  const [value, setValue] = useState<Loadable<T>>({ status: 'loading' });
  const sequence = useRef(0);
  const reload = useCallback(async () => {
    const issued = ++sequence.current;
    const next = await load();
    if (issued !== sequence.current) return;
    setValue(next);
  }, [load]);
  return [value, reload];
}

export interface Dashboard {
  observed: Loadable<Map<string, AccountRow[]>>;
  accounts: Loadable<ClaudeStoreAccount[]>;
  codexAccounts: Loadable<CodexStoreAccount[]>;
  pool: Loadable<PoolProvider[]>;
  /** `null` means the section is hidden: `[server.status]` is opt-in. */
  status: StatusSource[] | null;
  reloadObserved: () => Promise<void>;
  reloadAccounts: () => Promise<void>;
  reloadCodexAccounts: () => Promise<void>;
  reloadPool: () => Promise<void>;
}

export function useDashboard(): Dashboard {
  const loadObserved = useCallback(async (): Promise<Loadable<Map<string, AccountRow[]>>> => {
    const observed = await readJson<{ accounts?: ObservedAccount[] }>(
      `${API}/observed`,
      'Failed to observe local accounts',
    );
    if (!observed.ok) return { status: 'error', message: observed.message };

    // Managed pool state only enriches this view, so each read stands alone: a
    // transient failure on either endpoint must not discard the other's result,
    // and neither may discard the observations themselves. `readJson` reports a
    // failure rather than throwing, which is what keeps that true through
    // `Promise.all`.
    const [pool, accounts] = await Promise.all([
      readJson<{ providers?: PoolProvider[] }>(`${API}/pool`, ''),
      readJson<{ accounts?: ClaudeStoreAccount[] }>(`${API}/accounts`, ''),
    ]);

    return {
      status: 'ready',
      data: accountGroups(
        observed.data.accounts ?? [],
        pool.ok ? pool.data : null,
        accounts.ok ? accounts.data : null,
      ),
    };
  }, []);

  const loadAccounts = useCallback(async (): Promise<Loadable<ClaudeStoreAccount[]>> => {
    const result = await readJson<{ accounts?: ClaudeStoreAccount[] }>(
      `${API}/accounts`,
      'Failed to load accounts',
    );
    return result.ok
      ? { status: 'ready', data: result.data.accounts ?? [] }
      : { status: 'error', message: result.message };
  }, []);

  const loadCodexAccounts = useCallback(async (): Promise<Loadable<CodexStoreAccount[]>> => {
    const result = await readJson<{ accounts?: CodexStoreAccount[] }>(
      `${API}/accounts/codex`,
      'Failed to load Codex accounts',
    );
    return result.ok
      ? { status: 'ready', data: result.data.accounts ?? [] }
      : { status: 'error', message: result.message };
  }, []);

  const loadPool = useCallback(async (): Promise<Loadable<PoolProvider[]>> => {
    const result = await readJson<{ providers?: PoolProvider[] }>(`${API}/pool`, 'Failed to load pool');
    return result.ok
      ? { status: 'ready', data: result.data.providers ?? [] }
      : { status: 'error', message: result.message };
  }, []);

  const [observed, reloadObserved] = useSequencedLoad(loadObserved);
  const [accounts, reloadAccounts] = useSequencedLoad(loadAccounts);
  const [codexAccounts, reloadCodexAccounts] = useSequencedLoad(loadCodexAccounts);
  const [pool, reloadPool] = useSequencedLoad(loadPool);

  // `[server.status]` is opt-in and observation-only. Zero configured sources
  // means the feature is off; a failed read is not worth an error row for a
  // section that reports nothing routing consults, so both hide it.
  const [status, setStatus] = useState<StatusSource[] | null>(null);

  useEffect(() => {
    void reloadObserved();
    void reloadAccounts();
    void reloadCodexAccounts();
    void reloadPool();
    void (async () => {
      const result = await readJson<{ sources?: StatusSource[] }>(`${API}/status`, '');
      const sources = result.ok ? (result.data.sources ?? []) : [];
      setStatus(sources.length ? sources : null);
    })();
  }, [reloadObserved, reloadAccounts, reloadCodexAccounts, reloadPool]);

  return {
    observed,
    accounts,
    codexAccounts,
    pool,
    status,
    reloadObserved,
    reloadAccounts,
    reloadCodexAccounts,
    reloadPool,
  };
}
