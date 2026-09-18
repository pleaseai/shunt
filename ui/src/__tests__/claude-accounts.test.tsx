import { cleanup, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { DEFAULT_EXPIRY_BUFFER_MS, headersOf, renderDashboard, rowOf, tbody } from '../test/harness';

const HOUR = 3_600_000;

function statusOf(name: string): HTMLElement {
  const status = rowOf(tbody('accounts').getByText(name)).querySelector('td.status');
  if (!status) throw new Error(`Claude row "${name}" has no status cell`);
  return status as HTMLElement;
}

describe('the Claude account store table', () => {
  /**
   * `expires_at` is the ~8h ACCESS-token deadline and means opposite things per
   * credential kind: an imported login carries a refresh token shunt renews
   * in-band, a setup token has none. Printing the same raw timestamp for both is
   * what made every healthy imported account read as expired within the hour.
   */
  it('reports a kind-derived status and keeps the raw expiry as a tooltip', async () => {
    const now = Date.now();
    await renderDashboard({
      accounts: [
        { name: 'imported-a', kind: 'imported', expires_at: now - HOUR, uuid: 'uuid-a' },
        { name: 'setup-b', kind: 'setup_token', expires_at: now + 30 * 24 * HOUR, uuid: null },
      ],
    });

    expect(headersOf('accounts')).toEqual(['Name', 'Kind', 'Status', 'UUID', '']);

    const imported = statusOf('imported-a');
    expect(imported).toHaveAttribute('data-state', 'available');
    expect(imported).toHaveTextContent('Auto-refreshes');
    expect(within(imported).getByText('shunt renews this login as needed')).toBeInTheDocument();
    // The timestamp is still reachable — as context, not as the verdict.
    expect(imported).toHaveAttribute('title', expect.stringContaining('access token expires'));

    const setup = statusOf('setup-b');
    expect(setup).toHaveAttribute('data-state', 'available');
    expect(setup).toHaveTextContent(/^Valid until /);
    expect(within(setup).getByText('Setup token · re-login before this date')).toBeInTheDocument();
  });

  /**
   * A setup token stops being usable one refresh buffer *before* its own
   * `expires_at`: `Tokens::is_valid_at` accepts a credential only while
   * `expires_at > now + EXPIRY_BUFFER`, and a setup token has no refresh token,
   * so inside that window a routed request already fails. Reporting it usable
   * there would have the dashboard contradict routing for exactly the five
   * minutes an operator needs the warning in — and an imported account must
   * never reach that verdict at all, whatever its timestamp says.
   */
  it('renders the expired state only for a setup token inside the refresh buffer', async () => {
    const now = Date.now();
    await renderDashboard({
      accounts: [
        // Long past its access-token deadline, and entirely healthy.
        { name: 'imported-stale', kind: 'imported', expires_at: now - 10 * HOUR },
        // Outside the buffer: still usable.
        { name: 'setup-outside', kind: 'setup_token', expires_at: now + DEFAULT_EXPIRY_BUFFER_MS + HOUR },
        // Inside the buffer: routing already refuses it.
        { name: 'setup-inside', kind: 'setup_token', expires_at: now + DEFAULT_EXPIRY_BUFFER_MS / 2 },
      ],
    });

    expect(statusOf('imported-stale')).toHaveTextContent('Auto-refreshes');
    expect(statusOf('imported-stale')).toHaveAttribute('data-state', 'available');
    expect(statusOf('setup-outside')).toHaveTextContent(/^Valid until /);

    const inside = statusOf('setup-inside');
    expect(inside).toHaveAttribute('data-state', 'expired');
    expect(inside).toHaveTextContent('Expired');
    expect(
      within(inside).getByText('Setup token cannot refresh · re-login required'),
    ).toBeInTheDocument();
  });

  /**
   * The boundary is the server's own `claude::auth::EXPIRY_BUFFER`, served by
   * `GET /admin/api/session` rather than copied into this bundle. Rendering the
   * same token against two served buffers is what proves the page reads the
   * served value: a hardcoded constant would answer identically to both.
   */
  it('draws the boundary at the refresh buffer the server serves', async () => {
    const expiresAt = Date.now() + 30 * 60_000;
    const account = { name: 'setup-b', kind: 'setup_token', expires_at: expiresAt };

    await renderDashboard({ session: { csrf: 'c', expiry_buffer_ms: HOUR }, accounts: [account] });
    // A one-hour buffer swallows a token 30 minutes from its deadline.
    expect(statusOf('setup-b')).toHaveTextContent('Expired');

    cleanup();
    await renderDashboard({ session: { csrf: 'c', expiry_buffer_ms: 0 }, accounts: [account] });
    // With no buffer the same token is still valid.
    expect(statusOf('setup-b')).toHaveTextContent(/^Valid until /);
  });

  /**
   * Re-login drives the add form rather than a dedicated endpoint: completing
   * the normal flow under an existing name overwrites that account in place. The
   * method must be preselected from the row's own kind — re-provisioning under
   * the other mode silently converts the account between refreshable and
   * inference-only.
   */
  it('offers every row a re-login that preselects that row’s own login method', async () => {
    const user = userEvent.setup();
    await renderDashboard({
      accounts: [
        { name: 'refreshable', kind: 'imported' },
        { name: 'inference-only', kind: 'setup_token' },
      ],
    });

    const name = document.getElementById('name') as HTMLInputElement;
    const oauth = document.getElementById('mode-oauth') as HTMLInputElement;
    const setup = document.getElementById('mode-setup') as HTMLInputElement;

    await user.click(
      within(rowOf(tbody('accounts').getByText('inference-only'))).getByRole('button', {
        name: 'Re-login',
      }),
    );
    expect(name.value).toBe('inference-only');
    expect(setup.checked).toBe(true);
    expect(oauth.checked).toBe(false);

    await user.click(
      within(rowOf(tbody('accounts').getByText('refreshable'))).getByRole('button', {
        name: 'Re-login',
      }),
    );
    expect(name.value).toBe('refreshable');
    expect(oauth.checked).toBe(true);
    expect(setup.checked).toBe(false);

    // Re-login is an addition to the row, not a replacement for Remove.
    for (const account of ['refreshable', 'inference-only']) {
      const row = within(rowOf(tbody('accounts').getByText(account)));
      expect(row.getByRole('button', { name: 'Remove' })).toBeInTheDocument();
    }
    // Only an imported login carries a refresh grant to probe.
    expect(
      within(rowOf(tbody('accounts').getByText('refreshable'))).getByRole('button', {
        name: 'Refresh',
      }),
    ).toBeInTheDocument();
    expect(
      within(rowOf(tbody('accounts').getByText('inference-only'))).queryByRole('button', {
        name: 'Refresh',
      }),
    ).toBeNull();
  });
});
