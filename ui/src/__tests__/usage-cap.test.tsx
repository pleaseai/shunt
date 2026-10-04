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

  it('reads the bar of the window that reached the cap as full when the pool reports the account capped', async () => {
    await renderDashboard(
      poolWith({
        capped: true,
        utilization_5h: 0.3,
        max_utilization_5h: 0.5,
        utilization_7d: 0.9,
        max_utilization_7d: 0.8,
      }),
    );
    // `capped` covers 5h and 7d together; only the window at its cap reads full.
    expect(bar('Week').track).toHaveAttribute('data-level', 'full');
    expect(bar('5h').track).not.toHaveAttribute('data-level');
  });

  it('reads the Fable bar as full from capped_fable alone', async () => {
    await renderDashboard(
      poolWith({ capped_fable: true, utilization_7d_oi: 0.6, max_utilization_fable: 0.5 }),
    );
    expect(bar('Fable').track).toHaveAttribute('data-level', 'full');
  });

  it('takes the full state from the server verdict, not from the value at the cap', async () => {
    // At the cap by the numbers, but the pool does not report the account capped.
    await renderDashboard(poolWith({ utilization_5h: 0.5, max_utilization_5h: 0.5 }));
    expect(bar('5h').track).not.toHaveAttribute('data-level');
  });

  it('does not read a client observation past the cap as full while the pool still selects the account', async () => {
    await renderDashboard({
      observed: [
        {
          provider: 'claude',
          source: 'claude code',
          identity: 'ops@example.com',
          detail: 'Max plan',
          state: 'available',
          uuid: 'acct-uuid-1',
          utilization_5h: 0.6,
        },
      ],
      accounts: [{ name: 'pool-a', kind: 'imported', uuid: 'acct-uuid-1' }],
      pool: poolWith({ utilization_5h: 0.3, max_utilization_5h: 0.5 }).pool,
    });
    const { caption, track } = bar('5h');
    // The folded row shows the client's newer value...
    expect(caption).toBe('60% used · cap 50%');
    // ...but selection reads the pool's 30%, so the account is not excluded.
    expect(track).not.toHaveAttribute('data-level');
  });

  it.each([0.1, 0.2, 0.45])(
    'reads a capped bar exactly at a %s cap as full despite float rounding',
    async (value) => {
      await renderDashboard(
        poolWith({ capped: true, utilization_5h: value, max_utilization_5h: value }),
      );
      expect(bar('5h').track).toHaveAttribute('data-level', 'full');
    },
  );

  it('treats a cap of zero as present', async () => {
    await renderDashboard(poolWith({ capped: true, utilization_5h: 0, max_utilization_5h: 0 }));
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
