import { cleanup, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';

import { renderDashboard, reply, rowOf, tbody } from '../test/harness';

/**
 * `[server.status]` — opt-in, observation-only, and the one section that is
 * absent rather than empty when the feature is off.
 *
 * Absence is the property worth pinning, and it is also the one a naive test
 * gets wrong: the section renders `null` while its fetch is in flight, exactly
 * as it does when nothing is configured, so "the section is not there" is true
 * of a *pending* dashboard too. `renderDashboard` waits for the heading when the
 * fixture configures sources, which is what makes the two distinguishable here.
 *
 * `useDashboard`'s third rule — a *failed* status read also hides the section,
 * because nothing here is consulted by routing — has no test, deliberately. It
 * resolves to `setStatus(null)`, which is the initial value, so the resulting
 * DOM is identical to both "not configured" and "still loading". There is no
 * observation that separates them, and a test that asserted absence after a
 * failing read would be passing for the pending-render reason above rather than
 * for the rule it named.
 */

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

it('hides the whole section when no status source is configured', async () => {
  await renderDashboard({ status: [] });

  expect(screen.queryByRole('heading', { name: 'Upstream status' })).not.toBeInTheDocument();
  expect(document.getElementById('status-section')).toBeNull();
});

it('reports each configured source with its indicator label', async () => {
  const sources = [
    { provider: 'anthropic', indicator: 'none', description: 'All Systems Operational' },
    { provider: 'openai', indicator: 'major', description: 'Elevated error rates' },
  ];
  await renderDashboard(
    { status: sources },
    {
      // Answer this one read slowly, so the section is still `null` when the
      // four tables have settled. That is the ordering a real server produces
      // whenever the Statuspage fetch behind `/admin/api/status` is the slowest
      // of the five, and without `renderDashboard`'s wait for this section the
      // assertions below run against a dashboard that has not rendered it yet.
      'GET /admin/api/status': () =>
        new Promise<Response>((resolve) => setTimeout(() => resolve(reply({ sources })), 40)),
    },
  );

  const anthropic = rowOf(tbody('status').getByText('anthropic'));
  expect(anthropic).toHaveTextContent('Operational');
  expect(anthropic).toHaveTextContent('All Systems Operational');

  const openai = rowOf(tbody('status').getByText('openai'));
  expect(openai).toHaveTextContent('Major outage');
});

/**
 * `error` wins over `description` in the cell, and is also the `title`, so the
 * server's single-line detail is reachable on hover. Pinned because the two
 * fields are both strings on the same cell — a swapped precedence would render
 * a healthy-looking description over a real failure.
 */
it('prefers a source error over its description', async () => {
  await renderDashboard({
    status: [
      {
        provider: 'anthropic',
        indicator: 'unknown',
        description: 'All Systems Operational',
        error: 'status endpoint timed out',
      },
    ],
  });

  const cell = tbody('status').getByText('status endpoint timed out');
  expect(cell).toHaveAttribute('title', 'status endpoint timed out');
  expect(tbody('status').queryByText('All Systems Operational')).not.toBeInTheDocument();
});
