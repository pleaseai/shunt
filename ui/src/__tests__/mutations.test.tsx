import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { App } from '../App';
import { mockApi, renderDashboard, reply, rowOf, tbody, unreadable } from '../test/harness';

const ACCOUNTS = {
  accounts: [{ name: 'pool-a', kind: 'imported', uuid: 'uuid-a' }],
  codexAccounts: [{ name: 'codex-a', account_id: 'acct-1' }],
};

function action(table: string, account: string, name: string): HTMLElement {
  return within(rowOf(tbody(table).getByText(account))).getByRole('button', { name });
}

describe('a store mutation re-reads the grouped table too', () => {
  beforeEach(() => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
  });

  /**
   * The grouped "Accounts and usage" table renders `needs_relogin` and the
   * coalesced managed/observed state, both of which any store mutation can
   * change. Refreshing only the store table the mutation touched leaves the
   * primary view — the one an operator actually reads — showing pre-mutation
   * state until a page reload.
   */
  it.each([
    [
      'removing a Claude account',
      'accounts',
      'pool-a',
      'Remove',
      { 'DELETE /admin/api/accounts/claude/pool-a': () => reply({}) },
    ],
    [
      'refreshing a Claude account',
      'accounts',
      'pool-a',
      'Refresh',
      { 'POST /admin/api/accounts/claude/pool-a/refresh': () => reply({ message: 'Refresh succeeded' }) },
    ],
    [
      'removing a Codex account',
      'codex-accounts',
      'codex-a',
      'Remove',
      { 'DELETE /admin/api/accounts/codex/codex-a': () => reply({}) },
    ],
  ])('re-reads it after %s', async (_label, table, account, button, routes) => {
    const user = userEvent.setup();
    const api = await renderDashboard(ACCOUNTS, routes);
    const before = api.callsTo('GET', '/admin/api/observed').length;

    await user.click(action(table, account, button));

    await waitFor(() =>
      expect(api.callsTo('GET', '/admin/api/observed').length).toBeGreaterThan(before),
    );
  });

  /**
   * The pool table has its own read, and request counting cannot prove it ran:
   * `reloadObserved` fetches `/admin/api/pool` too (for the coalescing), so the
   * endpoint's call count rises either way. Only `reloadPool` writes the
   * `pool` loadable the "Managed pool health" table renders from, so the
   * question is answered by what that table shows, not by who was called.
   */
  it('re-reads the managed pool health table after a store mutation', async () => {
    let plan = 'max';
    const user = userEvent.setup();
    await renderDashboard(
      { ...ACCOUNTS, pool: [] },
      {
        'GET /admin/api/pool': () =>
          reply({
            providers: [
              { provider: 'claude', auth: 'claude_oauth', accounts: [{ name: 'pool-a', plan }] },
            ],
          }),
        'DELETE /admin/api/accounts/claude/pool-a': () => {
          // The server's answer to the *next* pool read differs, so a table
          // still showing "Max" is one that never re-read.
          plan = 'team';
          return reply({});
        },
      },
    );
    expect(tbody('pool').getByText('Max')).toBeInTheDocument();

    await user.click(action('accounts', 'pool-a', 'Remove'));

    await waitFor(() => expect(tbody('pool').getByText('Team')).toBeInTheDocument());
  });

  /**
   * A refresh probe can *set* `needs_relogin` as easily as clear it, so a failed
   * probe changes the primary table just as much as a successful one. Skipping
   * the re-read on failure is what left the top table showing the pre-probe
   * state.
   */
  it('re-reads it after a refresh probe that fails, too', async () => {
    const user = userEvent.setup();
    const api = await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude/pool-a/refresh': () =>
        reply({ error: { message: 'the grant was rejected' } }, 400),
    });
    const before = api.callsTo('GET', '/admin/api/observed').length;

    await user.click(action('accounts', 'pool-a', 'Refresh'));

    await waitFor(() =>
      expect(api.callsTo('GET', '/admin/api/observed').length).toBeGreaterThan(before),
    );
    expect(document.getElementById('addmsg')).toHaveTextContent('the grant was rejected');
  });

  /** A declined confirmation is not a mutation: nothing is sent, nothing re-read. */
  it('sends nothing when the removal confirmation is declined', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);
    const user = userEvent.setup();
    const api = await renderDashboard(ACCOUNTS);
    const before = api.calls.length;

    await user.click(action('accounts', 'pool-a', 'Remove'));

    expect(api.calls.length).toBe(before);
  });

  /**
   * A refresh probe can succeed against an account the pool still considers
   * dead. The response says so, and the message must carry that verdict rather
   * than claiming a recovery the pool table would contradict.
   */
  it('reports a probe that succeeded against a still-dead account as a failure', async () => {
    const user = userEvent.setup();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude/pool-a/refresh': () =>
        reply({ message: 'Refresh succeeded, but the account is still benched', needs_relogin: true }),
    });

    await user.click(action('accounts', 'pool-a', 'Refresh'));

    await waitFor(() =>
      expect(document.getElementById('addmsg')).toHaveTextContent('still benched'),
    );
    expect(document.getElementById('addmsg')).toHaveClass('err');
  });
});

describe('the session bootstrap', () => {
  /**
   * The shell is served to anyone — it carries no operator data — so an
   * unauthenticated visitor reaches this bundle and only then learns they are
   * not signed in. The sign-in page is where that belongs, which is what the
   * server-rendered `/admin` does with a redirect.
   */
  it('sends an unauthenticated visitor to the sign-in page', async () => {
    const assign = vi.fn();
    vi.spyOn(window, 'location', 'get').mockReturnValue({
      ...window.location,
      assign,
    } as unknown as Location);

    mockApi({
      'GET /admin/api/session': () => reply({ error: { message: 'unauthorized' } }, 401),
    });
    render(<App />);

    await waitFor(() => expect(assign).toHaveBeenCalledWith('/admin/login'));
  });

  /**
   * The same redirect, with the body a reverse proxy in front of the admin
   * surface actually sends: an HTML 401 page. The status is the only part of
   * that answer the page can act on, so losing it to the body parse is what
   * would strand an unauthenticated operator on the loading state forever.
   */
  it('sends the visitor to sign in even when the 401 body is not JSON', async () => {
    const assign = vi.fn();
    vi.spyOn(window, 'location', 'get').mockReturnValue({
      ...window.location,
      assign,
    } as unknown as Location);

    mockApi({ 'GET /admin/api/session': () => unreadable(401) });
    render(<App />);

    await waitFor(() => expect(assign).toHaveBeenCalledWith('/admin/login'));
  });

  /** Any other bootstrap failure is reported rather than redirected away. */
  it('reports a bootstrap failure that is not an authentication problem', async () => {
    mockApi({
      'GET /admin/api/session': () => reply({ error: { message: 'the store is unreadable' } }, 500),
    });
    render(<App />);

    expect(await screen.findByText('the store is unreadable')).toBeInTheDocument();
  });
});
