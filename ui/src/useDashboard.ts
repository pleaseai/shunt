import { useCallback, useEffect, useRef, useState } from 'react';

import { accountGroups } from './accounts';
import { API, readJson } from './api';
import { useSession } from './session';
import type {
  AccountRow,
  AntigravityStoreAccount,
  ClaudeStoreAccount,
  CodexStoreAccount,
  ObservedAccount,
  PoolData,
  PoolProvider,
  StatusSource,
  XaiStoreAccount,
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
function useSequencedLoad<T, A extends unknown[] = []>(
  load: (...args: A) => Promise<Loadable<T>>,
): [Loadable<T>, (...args: A) => Promise<void>] {
  const [value, setValue] = useState<Loadable<T>>({ status: 'loading' });
  const sequence = useRef(0);
  const reload = useCallback(
    async (...args: A) => {
      const issued = ++sequence.current;
      const next = await load(...args);
      if (issued !== sequence.current) return;
      setValue(next);
    },
    [load],
  );
  return [value, reload];
}

/**
 * How often the usage table re-reads shunt's own pool state while the page is
 * visible. Cheap by construction: the automatic reload reuses the last local
 * observations (see `loadObserved`) and never asks a provider for anything.
 */
export const AUTO_REFRESH_MS = 30_000;

/** A full load's observations, and the pool state that same load read. */
interface ReusedObservations {
  observations: ObservedAccount[];
  pool: { providers?: PoolProvider[] } | null;
}

export interface Dashboard {
  observed: Loadable<Map<string, AccountRow[]>>;
  accounts: Loadable<ClaudeStoreAccount[]>;
  codexAccounts: Loadable<CodexStoreAccount[]>;
  antigravityAccounts: Loadable<AntigravityStoreAccount[]>;
  pool: Loadable<PoolData>;
  /** `null` means the section is hidden: `[server.status]` is opt-in. */
  status: StatusSource[] | null;
  /** Pass `{ reuseObservations: true }` to re-read pool state without re-observing. */
  reloadObserved: (options?: { reuseObservations?: boolean }) => Promise<void>;
  reloadAccounts: () => Promise<void>;
  reloadCodexAccounts: () => Promise<void>;
  reloadAntigravityAccounts: () => Promise<void>;
  reloadPool: () => Promise<void>;
  /** When the usage table last finished loading (epoch ms), or `null` before the first. */
  updatedAt: number | null;
  /** Repeat the whole page load, observations included. */
  refreshNow: () => void;
}

export function useDashboard(): Dashboard {
  const { hideObserved } = useSession();

  // The last full load's observations, for automatic reloads that must not
  // repeat them: `GET /admin/api/observed` reads every provider login on the
  // host and asks each provider's usage API about it, uncached, so a timer must
  // never drive it. Only an explicit load (page open, Reload, a mutation)
  // observes. The pool state that load read is kept with them: a timed reload
  // compares against it to tell what the pool has learned since.
  const lastObservations = useRef<ReusedObservations | null>(null);
  // Full loads in flight, counted until the whole load settles. A timed reload
  // issued meanwhile would win the sequence with the *previous* observations
  // and discard the fresh ones, so the timer leaves the usage table alone while
  // this is non-zero.
  const observing = useRef(0);
  // Which full load may write `lastObservations`: the latest one started. An
  // older one that settles later is discarded on screen by `useSequencedLoad`,
  // so it must not leave its observations (or its failure) for the timer either.
  const observeGeneration = useRef(0);
  // Whether the timer's previous reload is still in flight.
  const quietInFlight = useRef(false);

  const loadObserved = useCallback(
    async (options?: {
      reuseObservations?: boolean;
    }): Promise<Loadable<Map<string, AccountRow[]>>> => {
      // Under `[server.admin] hide_observed` the endpoint answers an empty list
      // by contract, so the read is skipped rather than made for nothing. The
      // table still renders: managed pool accounts are not observations.
      const reused = hideObserved
        ? null
        : options?.reuseObservations
          ? lastObservations.current
          : null;
      const full = !hideObserved && reused === null;
      const generation = full ? ++observeGeneration.current : 0;
      if (full) observing.current += 1;
      try {
        let observations: ObservedAccount[] = reused?.observations ?? [];
        if (full) {
          const observed = await readJson<{ accounts?: ObservedAccount[] }>(
            `${API}/observed`,
            'Failed to observe local accounts',
          );
          if (!observed.ok) {
            // Forget the older observations too, so the timer leaves the
            // error on screen instead of repainting it from stale logins.
            if (generation === observeGeneration.current) lastObservations.current = null;
            return { status: 'error', message: observed.message };
          }
          observations = observed.data.accounts ?? [];
        }

        // Managed pool state only enriches this view, so each read stands
        // alone: a transient failure on either endpoint must not discard the
        // other's result, and neither may discard the observations themselves.
        // `readJson` reports a failure rather than throwing, which is what keeps
        // that true through `Promise.all`.
        const [pool, accounts, codexAccounts, xaiAccounts] = await Promise.all([
          readJson<{ providers?: PoolProvider[] }>(`${API}/pool`, ''),
          readJson<{ accounts?: ClaudeStoreAccount[] }>(`${API}/accounts`, ''),
          readJson<{ accounts?: CodexStoreAccount[] }>(`${API}/accounts/codex`, ''),
          readJson<{ accounts?: XaiStoreAccount[] }>(`${API}/accounts/xai`, ''),
        ]);
        const poolData = pool.ok ? pool.data : null;
        if (full && generation === observeGeneration.current) {
          lastObservations.current = { observations, pool: poolData };
        }

        return {
          status: 'ready',
          data: accountGroups(
            observations,
            poolData,
            accounts.ok ? accounts.data : null,
            codexAccounts.ok ? codexAccounts.data : null,
            xaiAccounts.ok ? xaiAccounts.data : null,
            reused ? { reused: { pool: reused.pool } } : {},
          ),
        };
      } finally {
        if (full) observing.current -= 1;
      }
    },
    [hideObserved],
  );

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

  const loadAntigravityAccounts = useCallback(async (): Promise<
    Loadable<AntigravityStoreAccount[]>
  > => {
    const result = await readJson<{ accounts?: AntigravityStoreAccount[] }>(
      `${API}/accounts/antigravity`,
      'Failed to load Antigravity accounts',
    );
    return result.ok
      ? { status: 'ready', data: result.data.accounts ?? [] }
      : { status: 'error', message: result.message };
  }, []);

  const loadPool = useCallback(async (): Promise<Loadable<PoolData>> => {
    const result = await readJson<{
      providers?: PoolProvider[];
      sort_by_reset?: boolean;
      usage_poll?: { interval_seconds: number; last_started_at: number; next_at: number } | null;
    }>(`${API}/pool`, 'Failed to load pool');
    if (!result.ok) return { status: 'error', message: result.message };
    const poll = result.data.usage_poll;
    return {
      status: 'ready',
      data: {
        providers: result.data.providers ?? [],
        sortByReset: result.data.sort_by_reset ?? false,
        usagePoll: poll
          ? {
              intervalSeconds: poll.interval_seconds,
              lastStartedAt: poll.last_started_at,
              nextAt: poll.next_at,
            }
          : null,
      },
    };
  }, []);

  const [observed, reloadObserved] = useSequencedLoad(loadObserved);
  const [accounts, reloadAccounts] = useSequencedLoad(loadAccounts);
  const [codexAccounts, reloadCodexAccounts] = useSequencedLoad(loadCodexAccounts);
  const [antigravityAccounts, reloadAntigravityAccounts] = useSequencedLoad(loadAntigravityAccounts);
  const [pool, reloadPool] = useSequencedLoad(loadPool);

  // `[server.status]` is opt-in and observation-only. Zero configured sources
  // means the feature is off; a failed read is not worth an error row for a
  // section that reports nothing routing consults, so both hide it.
  const [status, setStatus] = useState<StatusSource[] | null>(null);
  const statusSequence = useRef(0);
  const reloadStatus = useCallback(async () => {
    const issued = ++statusSequence.current;
    const result = await readJson<{ sources?: StatusSource[] }>(`${API}/status`, '');
    if (issued !== statusSequence.current) return;
    const sources = result.ok ? (result.data.sources ?? []) : [];
    setStatus(sources.length ? sources : null);
  }, []);

  // The whole page load: run on mount, and again by Reload.
  const reloadAll = useCallback(() => {
    void reloadObserved();
    void reloadAccounts();
    void reloadCodexAccounts();
    void reloadAntigravityAccounts();
    void reloadPool();
    void reloadStatus();
  }, [
    reloadObserved,
    reloadAccounts,
    reloadCodexAccounts,
    reloadAntigravityAccounts,
    reloadPool,
    reloadStatus,
  ]);

  useEffect(() => {
    reloadAll();
  }, [reloadAll]);

  // Stamped whenever the usage table finishes a load, so the page can say how
  // fresh the numbers are. A superseded load never sets `observed`, so it never
  // stamps either.
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  useEffect(() => {
    if (observed.status === 'ready') setUpdatedAt(Date.now());
  }, [observed]);

  // Keep the usage table current while the page is in view: pool state only,
  // on a timer that is skipped while the tab is hidden, plus one catch-up read
  // when the tab comes back.
  useEffect(() => {
    const quiet = () => {
      // One automatic reload at a time. Each one supersedes the last, so on a
      // backend slower than the interval a new tick would discard every answer
      // before it landed and the table would never update.
      if (quietInFlight.current) return;
      const reloads = [reloadPool()];
      // A full load in flight is about to draw fresh observations; a quiet
      // reload issued now would win the sequence with the older ones.
      // With no observations to reuse (the last one failed), a quiet reload
      // would have to observe, and that is the call a timer must never make;
      // it would also paint over the error with a table that lacks the logins.
      if (observing.current === 0 && (hideObserved || lastObservations.current !== null)) {
        reloads.push(reloadObserved({ reuseObservations: true }));
      }
      quietInFlight.current = true;
      void Promise.allSettled(reloads).finally(() => {
        quietInFlight.current = false;
      });
    };
    const timer = setInterval(() => {
      if (document.visibilityState !== 'hidden') quiet();
    }, AUTO_REFRESH_MS);
    const onVisible = () => {
      if (document.visibilityState === 'visible') quiet();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [hideObserved, reloadObserved, reloadPool]);

  return {
    observed,
    accounts,
    codexAccounts,
    antigravityAccounts,
    pool,
    status,
    reloadObserved,
    reloadAccounts,
    reloadCodexAccounts,
    reloadAntigravityAccounts,
    reloadPool,
    updatedAt,
    refreshNow: reloadAll,
  };
}
