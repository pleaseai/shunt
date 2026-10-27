import { describe, expect, it } from 'vitest';

import { accountGroups } from '../accounts';
import type { ObservedAccount, PoolProvider } from '../types';

/**
 * How a reused observation folds into the pool account holding the same login,
 * read straight off `accountGroups` rather than through the page.
 */

const UUID = 'acct-uuid-1';

function observation(overrides: Partial<ObservedAccount> = {}): ObservedAccount {
  return {
    provider: 'claude',
    source: 'claude code',
    identity: 'ops@example.com',
    detail: null,
    state: 'available',
    uuid: UUID,
    utilization_5h: 0.5,
    reset_5h: 1_000,
    ...overrides,
  };
}

function pool(account: Record<string, unknown>): { providers: PoolProvider[] } {
  return {
    providers: [
      {
        provider: 'claude',
        auth: 'claude_oauth',
        accounts: [{ name: 'pool-a', has_state: true, ...account }],
      } as PoolProvider,
    ],
  };
}

const stores = { accounts: [{ name: 'pool-a', kind: 'imported', uuid: UUID }] };

function row(groups: ReturnType<typeof accountGroups>) {
  const [only] = groups.get('claude') ?? [];
  if (!only) throw new Error('no claude row');
  return only;
}

describe('a reused observation on a coalesced row', () => {
  it('keeps the full load numbers while the pool has not moved', () => {
    const current = pool({ utilization_5h: 0.25, reset_5h: 2_000 });
    const folded = row(
      accountGroups([observation()], current, stores, null, null, { reused: { pool: current } }),
    );
    expect(folded.utilization_5h).toBe(0.5);
    expect(folded.reset_5h).toBe(1_000);
  });

  it('takes the pool window once the pool has moved since that load', () => {
    const before = pool({ utilization_5h: 0.25, reset_5h: 2_000 });
    const now = pool({ utilization_5h: 0.75, reset_5h: 2_000 });
    const folded = row(
      accountGroups([observation()], now, stores, null, null, { reused: { pool: before } }),
    );
    expect(folded.utilization_5h).toBe(0.75);
  });

  /**
   * With no baseline (that load's pool read failed, or the account is new),
   * any pool value for the window is newer than the observation, a reset alone
   * included.
   */
  it('takes a pool reset over the observation when there is no baseline', () => {
    const now = pool({ utilization_5h: null, reset_5h: 2_000 });
    const folded = row(
      accountGroups([observation()], now, stores, null, null, { reused: { pool: null } }),
    );
    expect(folded.reset_5h).toBe(2_000);
    expect(folded.utilization_5h).toBeNull();
  });
});
