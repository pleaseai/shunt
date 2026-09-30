import { screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { headersOf, renderDashboard, rowOf, tbody, DEFAULT_EXPIRY_BUFFER_MS } from '../test/harness';

const ACCOUNTS = {
  accounts: [{ name: 'pool-a', kind: 'imported', uuid: 'uuid-a' }],
  codexAccounts: [{ name: 'codex-a', account_id: 'acct-1' }],
};

function session(access: 'read' | 'write') {
  return { csrf: 'test-csrf', expiry_buffer_ms: DEFAULT_EXPIRY_BUFFER_MS, access };
}

/** The row-action buttons rendered for one account, by accessible name. */
function actionsFor(table: string, account: string): string[] {
  return within(rowOf(tbody(table).getByText(account)))
    .queryAllByRole('button')
    .map((button) => button.textContent ?? '');
}

/**
 * `GET /admin/api/session` reports the tier the session authenticates with, and
 * a read session is one a `[server.admin] read_keys` login minted. The server
 * refuses its mutations with `403` either way (`require_write`) — what the
 * dashboard owes it is not offering the action in the first place.
 *
 * Both tiers are asserted against the same fixtures throughout: every property
 * here is about the *difference* the tier makes, and a read-only assertion
 * alone would also pass against a dashboard that rendered no actions at all.
 */
describe('a read-only admin session', () => {
  it('keeps every table it may read', async () => {
    await renderDashboard({ ...ACCOUNTS, session: session('read') });

    expect(screen.getByRole('heading', { name: 'Accounts and usage' })).toBeInTheDocument();
    expect(tbody('accounts').getByText('pool-a')).toBeInTheDocument();
    expect(tbody('codex-accounts').getByText('codex-a')).toBeInTheDocument();
  });

  it('offers no way to add an account, and says why', async () => {
    await renderDashboard({ ...ACCOUNTS, session: session('read') });

    expect(screen.queryByRole('heading', { name: 'Add Claude account' })).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Add Codex account' })).toBeNull();
    expect(screen.getByText(/read-only admin session/i)).toBeInTheDocument();
  });

  it('offers no row actions', async () => {
    await renderDashboard({ ...ACCOUNTS, session: session('read') });

    expect(actionsFor('accounts', 'pool-a')).toEqual([]);
    expect(actionsFor('codex-accounts', 'codex-a')).toEqual([]);
  });

  /**
   * The actions column goes with its buttons. A `<th>` left standing would put
   * the header row one cell wider than every body row, which is a rendering
   * bug rather than a cosmetic one.
   */
  it('drops the actions column rather than leaving it empty', async () => {
    await renderDashboard({ ...ACCOUNTS, session: session('read') });

    expect(headersOf('accounts')).toEqual(['Name', 'Kind', 'Status', 'UUID']);
    expect(headersOf('codex-accounts')).toEqual(['Name', 'Status', 'Account ID']);
  });
});

/**
 * The twin of every assertion above. Without it each one would also hold for a
 * dashboard that had simply lost its write affordances for everyone.
 */
describe('a write session', () => {
  it('keeps the add forms, the row actions, and the actions column', async () => {
    await renderDashboard({ ...ACCOUNTS, session: session('write') });

    expect(screen.getByRole('heading', { name: 'Add Claude account' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Add Codex account' })).toBeInTheDocument();
    expect(screen.queryByText(/read-only admin session/i)).toBeNull();

    expect(actionsFor('accounts', 'pool-a')).toEqual(['Refresh', 'Re-login', 'Remove']);
    expect(actionsFor('codex-accounts', 'codex-a')).toEqual(['Re-login', 'Remove']);

    expect(headersOf('accounts')).toEqual(['Name', 'Kind', 'Status', 'UUID', '']);
    expect(headersOf('codex-accounts')).toEqual(['Name', 'Status', 'Account ID', '']);
  });
});
