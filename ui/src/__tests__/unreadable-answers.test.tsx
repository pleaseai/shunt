import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { renderDashboard, reply, unreadable, type Route } from '../test/harness';

const ACCOUNTS = { accounts: [{ name: 'other-claude', kind: 'imported' }] };

/** Start a Claude flow and enter a code, leaving only the completion to fire. */
async function armCompletion(user: ReturnType<typeof userEvent.setup>): Promise<void> {
  await user.type(document.getElementById('name') as HTMLInputElement, 'first');
  await user.click(document.getElementById('start') as HTMLButtonElement);
  await screen.findByRole('link', { name: 'https://auth.example/claude' });
  await user.type(document.getElementById('code') as HTMLInputElement, 'auth-code');
}

describe('an answer this surface cannot read is not a verdict', () => {
  /**
   * The authorization code is single-use, so "it failed" and "the outcome is
   * unknown" are different instructions to an operator: the first invites a
   * retry that cannot succeed if the exchange already landed. A body the page
   * cannot parse — a proxy's error page in front of the admin surface, a
   * truncated response — carries no verdict, so it must reach the same
   * unknown-outcome path an abandoned request does, tables re-read and all.
   * `src/admin/script.rs` draws this line with a bare `await res.json()` in its
   * completion handler and `.catch(() => ({}))` in remove/refresh.
   */
  it('reports an unreadable completion answer as unknown and re-reads the tables', async () => {
    const user = userEvent.setup();
    const api = await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' })) as Route,
      'POST /admin/api/accounts/claude/first/complete': (() => unreadable(502)) as Route,
    });

    await armCompletion(user);
    const before = api.callsTo('GET', '/admin/api/accounts').length;
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    expect(document.getElementById('addmsg')).toHaveTextContent(
      'the account may still have been stored',
    );
    // The tables are the authority the message sends the operator to, so they
    // must actually be re-read before it says so.
    expect(api.callsTo('GET', '/admin/api/accounts').length).toBeGreaterThan(before);
  });

  /**
   * The same body on a 200. `response.ok` alone would call this "Account
   * stored" — a confident success for an exchange whose result nothing here
   * has actually seen.
   */
  it('does not claim success when a 200 completion answer is unreadable', async () => {
    const user = userEvent.setup();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' })) as Route,
      'POST /admin/api/accounts/claude/first/complete': (() => unreadable(200)) as Route,
    });

    await armCompletion(user);
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    expect(document.getElementById('addmsg')).not.toHaveTextContent('Account stored');
    expect(document.getElementById('addmsg')).toHaveTextContent(
      'the account may still have been stored',
    );
  });

  /**
   * A start whose answer cannot be read has no `authorize_url` to show, so
   * saying nothing leaves the operator waiting on a step that never opens.
   * The status is 200 deliberately: on an error status `response.ok` already
   * drives the failure branch, so only a readable-looking success separates a
   * page that checks whether it understood the answer from one that does not.
   */
  it('reports an unreadable start answer instead of leaving the form dead', async () => {
    const user = userEvent.setup();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() => unreadable(200)) as Route,
    });

    await user.type(document.getElementById('name') as HTMLInputElement, 'first');
    await user.click(document.getElementById('start') as HTMLButtonElement);

    expect(document.getElementById('step2')).toBeNull();
    expect(document.getElementById('addmsg')).toHaveTextContent('Failed to start');
  });
});
