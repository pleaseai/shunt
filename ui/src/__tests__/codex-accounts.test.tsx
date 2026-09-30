import { within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { headersOf, renderDashboard, reply, rowOf, tbody } from '../test/harness';

function statusOf(name: string): HTMLElement {
  const status = rowOf(tbody('codex-accounts').getByText(name)).querySelector('td.status');
  if (!status) throw new Error(`Codex row "${name}" has no status cell`);
  return status as HTMLElement;
}

describe('the Codex account store table', () => {
  /**
   * Unlike the Claude table's kind-derived status, this column is
   * unconditional: every Codex store account is refreshable — both writers into
   * the store reject a missing refresh token and there is no setup-token analog
   * — so shunt owns renewal for all of them. That expiry is never the operator's
   * problem, and printing it raw made every healthy account read as broken
   * within the hour.
   */
  it('reports who owns renewal, not the raw expiry', async () => {
    const past = Date.now() - 6 * 3_600_000;
    await renderDashboard({
      codexAccounts: [
        { name: 'codex-a', expires_at: past, account_id: 'acct-1' },
        { name: 'codex-b', expires_at: null, account_id: null },
      ],
    });

    expect(headersOf('codex-accounts')).toEqual(['Name', 'Status', 'Account ID', '']);

    for (const name of ['codex-a', 'codex-b']) {
      const status = statusOf(name);
      expect(status).toHaveAttribute('data-state', 'available');
      expect(status).toHaveTextContent('Auto-refreshes');
      expect(within(status).getByText('shunt renews this login as needed')).toBeInTheDocument();
    }

    // The timestamp survives only as out-of-band context, and an account with
    // none carries no tooltip at all rather than an empty one.
    expect(statusOf('codex-a')).toHaveAttribute(
      'title',
      expect.stringContaining('access token expires'),
    );
    expect(statusOf('codex-b')).not.toHaveAttribute('title');
    expect(tbody('codex-accounts').getByText('acct-1')).toBeInTheDocument();
  });

  /**
   * The Codex counterpart of the Claude re-login, and safe for the same reason:
   * completion overwrites the account in place. Any half-finished flow must be
   * cleared first so the operator cannot paste a code belonging to a different
   * account.
   */
  it('offers a re-login that clears a half-finished flow before re-priming', async () => {
    const user = userEvent.setup();
    await renderDashboard(
      {
        codexAccounts: [{ name: 'codex-a', account_id: 'acct-1' }],
      },
      {
        'POST /admin/api/accounts/codex': () =>
          reply({ name: 'first', authorize_url: 'https://auth.example/codex' }),
      },
    );

    const name = document.getElementById('codex-name') as HTMLInputElement;
    await user.type(name, 'first');
    await user.click(document.getElementById('start-codex') as HTMLButtonElement);

    // The authorization step is open and carries a pasted code.
    const code = (await within(document.getElementById('codex-step2') as HTMLElement).findByRole(
      'textbox',
    )) as HTMLTextAreaElement;
    await user.type(code, 'https://localhost:1455/auth/callback?code=abc');
    expect(code.value).not.toBe('');

    await user.click(
      within(rowOf(tbody('codex-accounts').getByText('codex-a'))).getByRole('button', {
        name: 'Re-login',
      }),
    );

    expect(name.value).toBe('codex-a');
    // Step 2 is gone, and with it the code that belonged to the other account.
    expect(document.getElementById('codex-step2')).toBeNull();
    expect(document.getElementById('codex-code')).toBeNull();
  });
});
