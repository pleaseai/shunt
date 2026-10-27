import { act, cleanup, fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ObservedAccount } from '../types';
import { AUTO_REFRESH_MS } from '../useDashboard';
import { renderDashboard, reply, rowOf, tbody } from '../test/harness';

/**
 * The usage table keeps itself current, and says how current it is.
 *
 * The property that matters most is what the timer does NOT do:
 * `GET /admin/api/observed` reads every provider login on the host and asks
 * each provider's usage API about it, uncached, so a page left open must never
 * drive it. The timer re-reads shunt's own pool state only; an explicit Reload
 * repeats the full page load.
 */

const OBSERVED = '/admin/api/observed';
const POOL = '/admin/api/pool';

let visibility: DocumentVisibilityState = 'visible';

beforeEach(() => {
  // Real time keeps flowing, so the harness's async waits still settle;
  // `advanceTimersByTimeAsync` jumps the auto-refresh timer on demand.
  vi.useFakeTimers({ shouldAdvanceTime: true });
  visibility = 'visible';
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => visibility,
  });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  // Back to jsdom's own getter on the prototype.
  delete (document as { visibilityState?: unknown }).visibilityState;
});

function login(): ObservedAccount {
  return {
    provider: 'claude',
    source: 'client',
    identity: 'someone@example.com',
    detail: null,
    state: 'available',
    quota_buckets: [],
    utilization_5h: 0.25,
    reset_5h: null,
    utilization_7d: 0.5,
    reset_7d: null,
    utilization_7d_oi: null,
    reset_7d_oi: null,
  };
}

function setVisibility(next: DocumentVisibilityState): void {
  visibility = next;
  document.dispatchEvent(new Event('visibilitychange'));
}

describe('the usage table refreshes itself', () => {
  it('re-reads pool state on a timer without observing again', async () => {
    const api = await renderDashboard({ observed: [login()] });
    const observed = api.callsTo('GET', OBSERVED).length;
    const pool = api.callsTo('GET', POOL).length;
    expect(observed).toBe(1);

    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    await waitFor(() => expect(api.callsTo('GET', POOL).length).toBeGreaterThan(pool));
    expect(api.callsTo('GET', OBSERVED)).toHaveLength(observed);
    // The reused observation still renders, so the table did not fall back to
    // the pool's rows alone.
    expect(screen.getByText('someone@example.com')).toBeInTheDocument();
  });

  it('skips the timer while the tab is hidden and catches up once when it returns', async () => {
    const api = await renderDashboard({ observed: [login()] });
    const pool = api.callsTo('GET', POOL).length;

    setVisibility('hidden');
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS * 3));
    expect(api.callsTo('GET', POOL)).toHaveLength(pool);

    act(() => setVisibility('visible'));
    await waitFor(() => expect(api.callsTo('GET', POOL).length).toBeGreaterThan(pool));
    expect(api.callsTo('GET', OBSERVED)).toHaveLength(1);
  });

  it('never observes from the timer, even when the last observation failed', async () => {
    const api = await renderDashboard({}, { [`GET ${OBSERVED}`]: () => reply({}, 500) });
    expect(await screen.findByText('Failed to observe local accounts')).toBeInTheDocument();
    const pool = api.callsTo('GET', POOL).length;

    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    // The pool table still refreshes; the failed usage table is left showing
    // its error rather than repainted, and nothing asks to observe again.
    await waitFor(() => expect(api.callsTo('GET', POOL).length).toBeGreaterThan(pool));
    expect(api.callsTo('GET', OBSERVED)).toHaveLength(1);
    expect(screen.getByText('Failed to observe local accounts')).toBeInTheDocument();
  });

  /**
   * A managed account that is also this host's login is one row, and on a full
   * load its windows come from the login's observation. A timed reload reuses
   * that observation, so it must not paint the old numbers over the pool's new
   * ones, or the row freezes while "Updated" keeps ticking.
   */
  it('shows fresh pool usage on a coalesced row after a timed reload', async () => {
    let poolUtilization = 0.25;
    await renderDashboard(
      {
        observed: [{ ...login(), uuid: 'acct-uuid-1', utilization_5h: 0.25 }],
        accounts: [{ name: 'pool-a', kind: 'imported', uuid: 'acct-uuid-1' }],
      },
      {
        [`GET ${POOL}`]: () =>
          reply({
            providers: [
              {
                provider: 'claude',
                auth: 'claude_oauth',
                accounts: [{ name: 'pool-a', has_state: true, utilization_5h: poolUtilization }],
              },
            ],
          }),
      },
    );
    const row = () => rowOf(tbody('observed').getByText('pool-a'));
    expect(row()).toHaveTextContent('25%');

    poolUtilization = 0.75;
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    await waitFor(() => expect(row()).toHaveTextContent('75%'));
  });

  /**
   * The other half: the observation asked the provider directly, so it can be
   * newer than the pool. While the pool has learned nothing since that load, a
   * timed reload must not roll the row back to the pool's older number.
   */
  it('keeps a coalesced row newer observation while the pool has not moved', async () => {
    await renderDashboard(
      {
        observed: [{ ...login(), uuid: 'acct-uuid-1', utilization_5h: 0.75 }],
        accounts: [{ name: 'pool-a', kind: 'imported', uuid: 'acct-uuid-1' }],
      },
      {
        [`GET ${POOL}`]: () =>
          reply({
            providers: [
              {
                provider: 'claude',
                auth: 'claude_oauth',
                accounts: [{ name: 'pool-a', has_state: true, utilization_5h: 0.25 }],
              },
            ],
          }),
      },
    );
    const row = () => rowOf(tbody('observed').getByText('pool-a'));
    expect(row()).toHaveTextContent('75%');

    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    expect(row()).toHaveTextContent('75%');
    expect(row()).not.toHaveTextContent('25%');
  });

  /**
   * A Codex login reads "Waiting for traffic" until a response carries its
   * quota. Once the pool has seen that traffic, a timed reload must not keep
   * repeating the cached claim next to the pool's fresh usage.
   */
  it('drops a cached "Waiting for traffic" once the pool has seen traffic', async () => {
    let poolAccount: Record<string, unknown> = { name: 'codex-a', has_state: false };
    await renderDashboard(
      {
        observed: [
          {
            provider: 'codex',
            source: 'local credential file',
            identity: 'ChatGPT · Pro',
            detail: null,
            state: 'waiting-for-traffic',
            signal: 'response-derived',
            uuid: 'chatgpt-account-id',
          },
        ],
        codexAccounts: [{ name: 'codex-a', account_id: 'chatgpt-account-id' }],
      },
      {
        [`GET ${POOL}`]: () =>
          reply({
            providers: [{ provider: 'codex', auth: 'chatgpt_oauth', accounts: [poolAccount] }],
          }),
      },
    );
    const row = () => rowOf(tbody('observed').getByText('codex-a'));
    expect(row()).toHaveTextContent('Waiting for traffic');

    poolAccount = { name: 'codex-a', has_state: true, utilization_5h: 0.4 };
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    await waitFor(() => expect(row()).toHaveTextContent('40%'));
    expect(row()).not.toHaveTextContent('Waiting for traffic');
  });

  /**
   * A full load is in flight until its pool read settles, not just its
   * observation. A timer tick in that gap would win the sequence with the
   * previous observations and throw the fresh ones away.
   */
  it('does not let a timer tick during a full load paint the older observations', async () => {
    const someone = (identity: string): ObservedAccount => ({ ...login(), identity });
    let holdPool = false;
    const held: Array<() => void> = [];
    let observeCalls = 0;
    await renderDashboard(
      {},
      {
        [`GET ${OBSERVED}`]: () => {
          observeCalls += 1;
          return reply({ accounts: [someone(observeCalls === 1 ? 'old@example.com' : 'new@example.com')] });
        },
        [`GET ${POOL}`]: () => {
          const answer = reply({ providers: [] });
          if (!holdPool) return answer;
          return new Promise<Response>((resolve) => held.push(() => resolve(answer)));
        },
      },
    );
    expect(screen.getByText('old@example.com')).toBeInTheDocument();

    holdPool = true;
    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
    await waitFor(() => expect(observeCalls).toBe(2));
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));
    holdPool = false;
    await act(async () => {
      for (const release of held.splice(0)) release();
    });

    await waitFor(() => expect(screen.getByText('new@example.com')).toBeInTheDocument());
    expect(screen.queryByText('old@example.com')).not.toBeInTheDocument();
  });

  /**
   * On a backend slower than the interval, a tick that started a new reload
   * before the last one answered would supersede every answer, and the table
   * would never update. A tick waits for the previous one instead.
   */
  it('starts no new automatic reload while the previous one is in flight', async () => {
    let holdPool = false;
    const held: Array<() => void> = [];
    const api = await renderDashboard(
      { observed: [login()] },
      {
        [`GET ${POOL}`]: () => {
          const answer = reply({ providers: [] });
          if (!holdPool) return answer;
          return new Promise<Response>((resolve) => held.push(() => resolve(answer)));
        },
      },
    );
    const pool = api.callsTo('GET', POOL).length;

    holdPool = true;
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));
    await waitFor(() => expect(api.callsTo('GET', POOL).length).toBeGreaterThan(pool));
    const afterFirstTick = api.callsTo('GET', POOL).length;
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS * 2));
    expect(api.callsTo('GET', POOL)).toHaveLength(afterFirstTick);

    holdPool = false;
    await act(async () => {
      for (const release of held.splice(0)) release();
    });
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));
    await waitFor(() => expect(api.callsTo('GET', POOL).length).toBeGreaterThan(afterFirstTick));
  });

  it('observes again on an explicit Reload, and reloads every section', async () => {
    const api = await renderDashboard({ observed: [login()] });
    expect(api.callsTo('GET', OBSERVED)).toHaveLength(1);
    const stores = api.callsTo('GET', '/admin/api/accounts/antigravity').length;
    const status = api.callsTo('GET', '/admin/api/status').length;

    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));

    await waitFor(() => expect(api.callsTo('GET', OBSERVED)).toHaveLength(2));
    await waitFor(() => {
      expect(api.callsTo('GET', '/admin/api/accounts/antigravity')).toHaveLength(stores + 1);
      expect(api.callsTo('GET', '/admin/api/status')).toHaveLength(status + 1);
    });
  });

  /**
   * Two Reloads overlap and the older observation answers last. It loses on
   * screen; it must also lose the cache the timer reuses, or the next timed
   * reload brings the older logins back.
   */
  it('keeps the newest observations for the timer when an older load settles last', async () => {
    const someone = (identity: string): ObservedAccount => ({ ...login(), identity });
    let settleOlder: (response: Response) => void = () => {};
    let call = 0;
    await renderDashboard(
      {},
      {
        [`GET ${OBSERVED}`]: () => {
          call += 1;
          if (call === 2) return new Promise<Response>((resolve) => (settleOlder = resolve));
          return reply({ accounts: [someone(call === 1 ? 'first@example.com' : 'new@example.com')] });
        },
      },
    );

    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
    expect(await screen.findByText('new@example.com')).toBeInTheDocument();

    await act(async () => settleOlder(reply({ accounts: [someone('old@example.com')] })));
    await act(() => vi.advanceTimersByTimeAsync(AUTO_REFRESH_MS));

    await waitFor(() => expect(call).toBe(3));
    expect(screen.getByText('new@example.com')).toBeInTheDocument();
    expect(screen.queryByText('old@example.com')).not.toBeInTheDocument();
  });
});

describe('the refresh bar', () => {
  function poolWithPoll(nextIn: number | null) {
    const now = Math.floor(Date.now() / 1000);
    return {
      [`GET ${POOL}`]: () =>
        reply({
          providers: [],
          usage_poll:
            nextIn === null
              ? null
              : { interval_seconds: 300, last_started_at: now + nextIn - 300, next_at: now + nextIn },
        }),
    };
  }

  it('counts down to the next quota check', async () => {
    await renderDashboard({}, poolWithPoll(125));
    expect(await screen.findByText(/^Next quota check in 2:0[45]$/)).toBeInTheDocument();
    expect(screen.getByText(/^Updated \d+s ago$/)).toBeInTheDocument();
  });

  it('says a check is due once the scheduled time has passed', async () => {
    await renderDashboard({}, poolWithPoll(-5));
    expect(await screen.findByText('Quota check due')).toBeInTheDocument();
  });

  it('shows no countdown when the usage poller is off', async () => {
    await renderDashboard({}, poolWithPoll(null));
    expect(screen.getByRole('button', { name: 'Reload' })).toBeInTheDocument();
    expect(screen.queryByText(/quota check/i)).not.toBeInTheDocument();
  });
});
