import { within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { renderDashboard, rowOf } from '../test/harness';
import type { ObservedAccount, PoolAccount, PoolProvider } from '../types';

const UUID = 'acct-uuid-1';

function observedClaude(overrides: Partial<ObservedAccount> = {}): ObservedAccount {
  return {
    provider: 'claude',
    source: 'claude code',
    identity: 'ops@example.com',
    detail: 'Max plan',
    state: 'available',
    uuid: UUID,
    ...overrides,
  };
}

function poolWith(auth: string, provider: string, account: Record<string, unknown>): PoolProvider[] {
  return [
    {
      provider,
      auth,
      accounts: [{ name: 'pool-a', has_state: true, ...account } as PoolAccount],
    },
  ];
}

/**
 * The primary "Accounts and usage" table. Scoped because an account name also
 * appears in the managed-pool table further down, and an unscoped query would
 * match either one.
 */
function observed() {
  const table = document.getElementById('observed-table');
  if (!table) throw new Error('the observed table is not rendered');
  return within(table);
}

/** The status cell of the observed row whose account label is `label`. */
function statusOf(label: string): HTMLElement {
  const status = rowOf(observed().getByText(label)).querySelector('td.status');
  if (!status) throw new Error(`row "${label}" has no status cell`);
  return status as HTMLElement;
}

describe('folding managed accounts and local observations into one row', () => {
  /**
   * The uuid table is built from the Claude account store alone, so it may only
   * be applied to `claude_oauth` accounts. Keying that decision on the provider
   * *name* is what breaks: a table can be named anything.
   */
  it('coalesces on the account auth kind, not on the provider display name', async () => {
    await renderDashboard({
      observed: [observedClaude()],
      accounts: [{ name: 'pool-a', kind: 'imported', uuid: UUID }],
      // Named "claude", but the accounts in it are ChatGPT logins — a store uuid
      // must not be matched against them.
      pool: poolWith('chatgpt_oauth', 'claude', {}),
    });

    // Two rows, not one: the store uuid was never applied to a ChatGPT login.
    expect(observed().getByText('pool-a')).toBeInTheDocument();
    expect(observed().getByText('ops@example.com')).toBeInTheDocument();
  });

  it('coalesces a claude_oauth table under a name of the operator’s choosing', async () => {
    await renderDashboard({
      observed: [observedClaude()],
      accounts: [{ name: 'pool-a', kind: 'imported', uuid: UUID }],
      pool: poolWith('claude_oauth', 'house-claude', {}),
    });

    // One account seen through two lenses is one row: the managed name wins as
    // the label, and the observation is folded into it.
    expect(observed().getByText('pool-a')).toBeInTheDocument();
    expect(observed().queryByText('ops@example.com')).toBeNull();
    expect(observed().getByText('pool-a').closest('td')).toHaveAttribute(
      'title',
      expect.stringContaining('same subscription as the local'),
    );
  });

  /**
   * The visible label, the `data-state` the stylesheet colours from, and the
   * remediation note are three renderings of one decision. Deriving them
   * separately is what produced a "Needs login" label beside a green dot with no
   * login hint.
   */
  it('gives the label, the styling hook and the remediation one effective state', async () => {
    await renderDashboard({
      observed: [observedClaude({ state: 'expired' })],
      accounts: [{ name: 'pool-a', kind: 'imported', uuid: UUID }],
      pool: poolWith('claude_oauth', 'anthropic', { needs_relogin: true }),
    });

    const status = statusOf('pool-a');
    expect(status).toHaveAttribute('data-state', 'needs-relogin');
    expect(status).toHaveTextContent('Needs re-login');
    expect(within(status).getByText('Re-add this account to sign in again')).toBeInTheDocument();
    // The observation's own verdict must not leak through as a second opinion.
    expect(status).not.toHaveTextContent('Needs login');
  });

  /**
   * A managed operational state is an actionable gateway-side fact; a local
   * observation error can be arbitrarily stale. An account the pool has already
   * benched must not read "Needs login" — with no cooldown remediation — just
   * because the last local check saw an expired token.
   */
  it.each([
    ['disabled', { disabled: true }, 'Disabled'],
    ['needs_relogin', { needs_relogin: true }, 'Needs re-login'],
    ['cooling', { cooldown_secs_remaining: 600 }, 'Cooling'],
    ['near_quota', { near_quota: true }, 'Near quota'],
    ['fable cooldown', { cooldown_fable_secs_remaining: 600 }, 'Cooling (Fable)'],
  ])('lets a managed %s outrank a stale observed error', async (_label, managed, expected) => {
    await renderDashboard({
      observed: [observedClaude({ state: 'expired' })],
      accounts: [{ name: 'pool-a', kind: 'imported', uuid: UUID }],
      pool: poolWith('claude_oauth', 'anthropic', managed),
    });

    const status = statusOf('pool-a');
    expect(status).toHaveTextContent(expected);
    expect(status).not.toHaveTextContent('Needs login');
  });

  /**
   * A Fable-only cooldown leaves the account fully usable for every other model,
   * so it needs a state of its own: reporting it as a plain "Cooling" reads as an
   * account that cannot serve anything.
   */
  it('surfaces a fable-only cooldown as its own state with its own remediation', async () => {
    await renderDashboard({
      accounts: [],
      pool: poolWith('claude_oauth', 'anthropic', { cooldown_fable_secs_remaining: 600 }),
    });

    const status = statusOf('pool-a');
    expect(status).toHaveAttribute('data-state', 'cooling-fable');
    expect(status).toHaveTextContent('Cooling (Fable)');
    expect(within(status).getByText(/^Fable retries in /)).toBeInTheDocument();
  });
});
