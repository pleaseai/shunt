import { describe, expect, it } from 'vitest';

import { renderDashboard, rowOf, tbody } from '../test/harness';
import type { PoolAccount } from '../types';

function poolWith(account: PoolAccount) {
  return { pool: [{ provider: 'claude', auth: 'claude_oauth', accounts: [account] }] };
}

/** The State cell of the pool row for `name`, read off the rendered table. */
function stateOf(name: string): string {
  const cells = rowOf(tbody('pool').getByText(name)).querySelectorAll('td');
  // Provider, Account, Plan, State — the header order the table declares.
  return cells[3]?.textContent ?? '';
}

describe('the pool table reports why an account is out, not just that it is', () => {
  /**
   * A dead credential is *also* cooling down, so a `poolState` that tested the
   * cooldown fields first would report "cooling" for both — which is what made
   * a permanently dead account indistinguishable from a quota pause. This table
   * runs its own precedence chain, separate from `effectiveState`, so the
   * ordering has to be pinned here rather than inherited from that one's tests.
   */
  it('reports a dead credential as needs re-login even while it is cooling down', async () => {
    await renderDashboard(
      poolWith({
        name: 'dead-one',
        has_state: true,
        needs_relogin: true,
        cooldown_secs_remaining: 300,
      }),
    );
    expect(stateOf('dead-one')).toBe('needs re-login');
  });

  /** The same account without the marker is the case the ordering must not eat. */
  it('still reports a merely cooling account as cooling', async () => {
    await renderDashboard(
      poolWith({ name: 'paused-one', has_state: true, cooldown_secs_remaining: 300 }),
    );
    expect(stateOf('paused-one')).toBe('cooling');
  });

  /** `disabled` is an operator's own decision and outranks everything observed. */
  it('reports an operator-disabled account as disabled over its live state', async () => {
    await renderDashboard(
      poolWith({
        name: 'off-one',
        disabled: true,
        has_state: true,
        needs_relogin: true,
        near_quota: true,
      }),
    );
    expect(stateOf('off-one')).toBe('disabled');
  });

  /** An account the pool has never seen answer is not "available". */
  it('reports an account with no recorded response as unseen', async () => {
    await renderDashboard(poolWith({ name: 'new-one', has_state: false }));
    expect(stateOf('new-one')).toBe('unseen');
  });

  /** Both cooldowns at once name both windows, so a Fable pause stays visible. */
  it('names the fable cooldown alongside the account-wide one', async () => {
    await renderDashboard(
      poolWith({
        name: 'both-one',
        has_state: true,
        cooldown_secs_remaining: 300,
        cooldown_fable_secs_remaining: 60,
      }),
    );
    const cells = rowOf(tbody('pool').getByText('both-one')).querySelectorAll('td');
    expect(cells[8]?.textContent).toBe('300s · 60s (fable)');
  });
});
