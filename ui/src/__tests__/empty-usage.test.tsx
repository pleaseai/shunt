import { cleanup, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ObservedAccount } from '../types';
import { renderDashboard, rowOf, tbody } from '../test/harness';

/**
 * The Usage cell's empty-state copy, for an account with no quota data at all.
 *
 * This is the one property of the server-rendered dashboard's fifteen that
 * PR #508 did not carry forward, so it is the gap this file closes. The copy is
 * not decoration: it is the whole content of the cell for an account the
 * gateway cannot report usage for, and each string names a *different* operator
 * action — renew a provider login, send a request, wait, or nothing at all. A
 * single wrong branch silently tells an operator to do the wrong thing, and
 * nothing else on the page distinguishes the four.
 *
 * The branches mirror the deleted `script.rs` exactly (`admin/script.rs:263-267`
 * at `eb25c35e`, the last commit that carried it) — including that its third
 * branch tested a `pending` local which was itself `state === "connected"`, so
 * `emptyUsageText`'s `'connected'` check is the same condition and not a
 * divergence introduced by the port.
 */

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** An observation with no buckets and no windows, so the Usage cell is empty. */
function withoutUsage(overrides: Partial<ObservedAccount>): ObservedAccount {
  return {
    provider: 'claude',
    source: 'client',
    identity: 'someone@example.com',
    detail: null,
    state: 'available',
    quota_buckets: [],
    utilization_5h: null,
    reset_5h: null,
    utilization_7d: null,
    reset_7d: null,
    utilization_7d_oi: null,
    reset_7d_oi: null,
    ...overrides,
  };
}

describe('the Usage cell with no quota data', () => {
  it.each([
    ['expired', { state: 'expired' }, 'Sign in again with the provider client'],
    [
      'waiting-for-traffic',
      { state: 'waiting-for-traffic' },
      'Send one GPT request through this shunt',
    ],
    ['connected', { signal: 'integration-pending' }, 'Usage integration in progress'],
    ['anything else', {}, 'No usage reported yet'],
  ])('tells a %s account what to do', async (_name, overrides, expected) => {
    await renderDashboard({ observed: [withoutUsage(overrides)] });

    const row = rowOf(tbody('observed').getByText('someone@example.com'));
    expect(row).toHaveTextContent(expected);
  });

  it('renders no usage bar, so the empty copy is the whole cell', async () => {
    await renderDashboard({ observed: [withoutUsage({})] });

    // Without this, a cell that rendered a 0%-wide bar *and* the copy would pass
    // every assertion above while showing an account as having reported zero
    // usage — which is a different claim from having reported none.
    expect(document.querySelectorAll('#observed .usage-track')).toHaveLength(0);
  });
});

/**
 * The table-level empty state, which is a different claim from a row with no
 * usage: no provider login was found at all, so there is no row to caption.
 */
it('tells an operator with no local provider login where to start', async () => {
  await renderDashboard({ observed: [] });

  expect(
    screen.getByText('No supported local provider login found. Sign in with a provider CLI.'),
  ).toBeInTheDocument();
});
