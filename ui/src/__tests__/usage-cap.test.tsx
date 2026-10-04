import { describe, expect, it } from 'vitest';

import { renderDashboard, rowOf, tbody } from '../test/harness';
import type { PoolAccount } from '../types';

function poolWith(account: Partial<PoolAccount>) {
  return {
    accounts: [],
    pool: [
      {
        provider: 'anthropic',
        auth: 'claude_oauth',
        accounts: [{ name: 'pool-a', has_state: true, ...account } as PoolAccount],
      },
    ],
  };
}

/** The usage bar of the grouped table whose label is `label`, with its caption and cap tick. */
function bar(label: string) {
  const track = document.querySelector(`#observed-table progress[aria-label="${label} usage"]`);
  if (!track) throw new Error(`no ${label} usage bar rendered`);
  const item = track.closest('.usage-item') as HTMLElement;
  return {
    track,
    caption: item.querySelector('.usage-value')?.textContent ?? '',
    tick: item.querySelector('.usage-cap'),
  };
}

describe('the configured hard cap is shown beside the usage it limits', () => {
  it('names the cap in the caption and draws a decorative tick at its position', async () => {
    await renderDashboard(
      poolWith({ utilization_5h: 0.3, max_utilization_5h: 0.5, max_utilization_7d: 0.8 }),
    );
    const { caption, tick, track } = bar('5h');
    expect(caption).toBe('30% used · cap 50%');
    expect(tick).toHaveAttribute('aria-hidden', 'true');
    expect((tick as HTMLElement).style.getPropertyValue('--cap')).toBe('50%');
    expect(track).not.toHaveAttribute('data-level');
    // The label the bar is found by is unchanged.
    expect(track).toHaveAttribute('aria-label', '5h usage');
  });

  it('reads a bar at or past its cap as full, since the account is excluded there', async () => {
    await renderDashboard(
      poolWith({
        utilization_5h: 0.5,
        max_utilization_5h: 0.5,
        utilization_7d: 0.4,
        max_utilization_7d: 0.8,
      }),
    );
    expect(bar('5h').track).toHaveAttribute('data-level', 'full');
    expect(bar('Week').track).not.toHaveAttribute('data-level');
  });

  it.each([0.1, 0.2, 0.45])(
    'reads a bar exactly at a %s cap as full despite float rounding',
    async (value) => {
      await renderDashboard(poolWith({ utilization_5h: value, max_utilization_5h: value }));
      expect(bar('5h').track).toHaveAttribute('data-level', 'full');
    },
  );

  it('treats a cap of zero as present', async () => {
    await renderDashboard(poolWith({ utilization_5h: 0, max_utilization_5h: 0 }));
    const { caption, tick, track } = bar('5h');
    expect(caption).toBe('0% used · cap 0%');
    expect(tick).not.toBeNull();
    expect(track).toHaveAttribute('data-level', 'full');
  });

  it('renders a bar without a cap exactly as before', async () => {
    await renderDashboard(poolWith({ utilization_5h: 0.3, max_utilization_5h: null }));
    const { caption, tick, track } = bar('5h');
    expect(caption).toBe('30% used');
    expect(tick).toBeNull();
    expect(track).not.toHaveAttribute('data-level');
  });

  it('shows each cap in the pool table, including for a window with no utilization yet', async () => {
    await renderDashboard(
      poolWith({ utilization_5h: 0.3, max_utilization_5h: 0.5, max_utilization_fable: 0.875 }),
    );
    const cells = rowOf(tbody('pool').getByText('pool-a')).querySelectorAll('td');
    // Provider, Account, Plan, State, 5h, 7d, Fable.
    expect(cells[4]?.textContent).toBe('30% · cap 50%');
    expect(cells[5]?.textContent).toBe('—');
    // To 0.1%, as the usage bar shows it, not rounded to a whole percent.
    expect(cells[6]?.textContent).toBe('— · cap 87.5%');
  });
});
