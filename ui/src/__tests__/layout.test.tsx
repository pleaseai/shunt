import { screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { renderDashboard, rowOf } from '../test/harness';

describe('dashboard layout', () => {
  /**
   * Usage is what an operator opens this page for; pool management is the rarer,
   * riskier task. Putting the forms first is what made the page read as a
   * provisioning tool that happened to show numbers.
   */
  it('leads with usage and keeps pool management collapsed behind a disclosure', async () => {
    await renderDashboard();

    const usage = screen.getByRole('heading', { name: 'Accounts and usage' });
    const disclosure = screen.getByText('Manage pool accounts').closest('details');
    expect(disclosure).not.toBeNull();
    expect(disclosure).not.toHaveAttribute('open');

    // Ordering, read off the document rather than off the source: the usage
    // heading precedes the disclosure, and every management section is inside
    // it.
    expect(usage.compareDocumentPosition(disclosure as Node)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
    for (const heading of [
      'Add Claude account',
      'Add Codex account',
      'Claude accounts',
      'Codex accounts',
      'Managed pool health',
    ]) {
      expect(within(disclosure as HTMLElement).getByRole('heading', { name: heading })).toBeInTheDocument();
    }

    // The pool table is titled by what it is, not by a name that competes with
    // the primary one.
    expect(screen.queryByRole('heading', { name: 'Pool health' })).toBeNull();
  });

  /**
   * The table names providers the way their vendors do and reports usage as a
   * bar an assistive technology can read, rather than as the raw signal names
   * the internal payload uses.
   */
  it('names providers as the vendor does and exposes usage as a labelled progressbar', async () => {
    await renderDashboard({
      observed: [
        {
          provider: 'codex',
          source: 'codex cli',
          identity: 'ops@example.com',
          detail: 'Pro plan',
          state: 'available',
          uuid: null,
          quota_buckets: [{ label: '5h', remaining: 0.25, reset_time: null }],
        },
      ],
    });

    // `codex` is the config key; GPT is what the operator recognizes.
    expect(screen.getByText('GPT')).toBeInTheDocument();
    expect(screen.queryByText('codex')).toBeNull();

    const bar = screen.getByRole('progressbar', { name: '5h usage' });
    expect(bar).toHaveAttribute('aria-valuenow', '75');
    expect(bar).toHaveAttribute('aria-valuemin', '0');
    expect(bar).toHaveAttribute('aria-valuemax', '100');
    expect(rowOf(bar)).toHaveTextContent('75% used');

    // The provider mark rides along with the name, inlined so the bundle makes
    // no external request, and hidden from assistive technology because the
    // adjacent text already says the same thing.
    const logo = document.querySelector('svg.provider-logo');
    expect(logo).not.toBeNull();
    expect(logo).toHaveAttribute('aria-hidden', 'true');

    // Column headers are the operator's vocabulary, not the payload's.
    const headers = screen
      .getAllByRole('columnheader')
      .map((header) => header.textContent?.trim());
    expect(headers).toContain('Usage');
    expect(headers).not.toContain('Signal');
    expect(headers).not.toContain('Resets');
  });
});
