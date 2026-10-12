import { within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { headersOf, renderDashboard, reply, rowOf, tbody } from '../test/harness';

function statusOf(name: string): HTMLElement {
  const status = rowOf(tbody('antigravity-accounts').getByText(name)).querySelector('td.status');
  if (!status) throw new Error(`Antigravity row "${name}" has no status cell`);
  return status as HTMLElement;
}

describe('the Antigravity account store table', () => {
  /**
   * Every Antigravity store account is refreshable — `store_oauth_tokens`
   * rejects a blank refresh token — so, like the Codex table, this column is
   * unconditional rather than kind-derived.
   */
  it('reports who owns renewal, not the raw expiry', async () => {
    const past = Date.now() - 6 * 3_600_000;
    await renderDashboard({
      antigravityAccounts: [
        { name: 'agy-a', expires_at: past, email: 'a@example.com' },
        { name: 'agy-b', expires_at: null, email: null },
      ],
    });

    expect(headersOf('antigravity-accounts')).toEqual(['Name', 'Status', 'Email', '']);

    for (const name of ['agy-a', 'agy-b']) {
      const status = statusOf(name);
      expect(status).toHaveAttribute('data-state', 'available');
      expect(status).toHaveTextContent('Auto-refreshes');
      expect(within(status).getByText('shunt renews this login as needed')).toBeInTheDocument();
    }

    expect(statusOf('agy-a')).toHaveAttribute('title', expect.stringContaining('access token expires'));
    expect(statusOf('agy-b')).not.toHaveAttribute('title');
    expect(tbody('antigravity-accounts').getByText('a@example.com')).toBeInTheDocument();
  });

  /**
   * The Antigravity counterpart of the Codex/Claude re-login: completion
   * overwrites the account in place, so any half-finished flow must be
   * cleared first, the operator cannot paste a code belonging to a different
   * account.
   */
  it('offers a re-login that clears a half-finished flow before re-priming', async () => {
    const user = userEvent.setup();
    await renderDashboard(
      {
        antigravityAccounts: [{ name: 'agy-a', email: 'a@example.com' }],
      },
      {
        'POST /admin/api/accounts/antigravity': () =>
          reply({ name: 'first', authorize_url: 'https://auth.example/antigravity' }),
      },
    );

    const name = document.getElementById('antigravity-name') as HTMLInputElement;
    await user.type(name, 'first');
    await user.click(document.getElementById('start-antigravity') as HTMLButtonElement);

    const code = (await within(
      document.getElementById('antigravity-step2') as HTMLElement,
    ).findByRole('textbox')) as HTMLTextAreaElement;
    await user.type(code, 'http://localhost:51121/oauth-callback?code=abc');
    expect(code.value).not.toBe('');

    await user.click(
      within(rowOf(tbody('antigravity-accounts').getByText('agy-a'))).getByRole('button', {
        name: 'Re-login',
      }),
    );

    expect(name.value).toBe('agy-a');
    expect(document.getElementById('antigravity-step2')).toBeNull();
    expect(document.getElementById('antigravity-code')).toBeNull();
  });

  /**
   * Unlike Codex, Antigravity's store exposes a real refresh probe
   * (`AntigravityAuthStore::force_refresh_if_access_token`), so this table
   * gets a Refresh button — mirroring the Claude table's success/failure
   * message rendering.
   */
  it('refreshes an account and reports the probe outcome', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const user = userEvent.setup();
    await renderDashboard(
      { antigravityAccounts: [{ name: 'agy-a', email: 'a@example.com' }] },
      {
        'POST /admin/api/accounts/antigravity/agy-a/refresh': () =>
          reply({ message: 'Refresh succeeded; this login is alive.', needs_relogin: false }),
      },
    );

    await user.click(
      within(rowOf(tbody('antigravity-accounts').getByText('agy-a'))).getByRole('button', {
        name: 'Refresh',
      }),
    );

    await within(document.getElementById('antigravity-addmsg') as HTMLElement).findByText(
      'Refresh succeeded; this login is alive.',
    );
    expect(document.getElementById('antigravity-addmsg')).toHaveClass('ok');
  });

  it('reports a failed refresh probe without a false success', async () => {
    const user = userEvent.setup();
    await renderDashboard(
      { antigravityAccounts: [{ name: 'agy-a', email: 'a@example.com' }] },
      {
        'POST /admin/api/accounts/antigravity/agy-a/refresh': () =>
          reply({ error: { message: 'this account needs a re-login' } }, 400),
      },
    );

    await user.click(
      within(rowOf(tbody('antigravity-accounts').getByText('agy-a'))).getByRole('button', {
        name: 'Refresh',
      }),
    );

    await within(document.getElementById('antigravity-addmsg') as HTMLElement).findByText(
      'this account needs a re-login',
    );
    expect(document.getElementById('antigravity-addmsg')).toHaveClass('err');
  });
});
