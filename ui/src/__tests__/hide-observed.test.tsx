import { cleanup, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { PoolAccount, PoolProvider } from '../types';
import { DEFAULT_EXPIRY_BUFFER_MS, renderDashboard, tbody } from '../test/harness';

/**
 * `[server.admin] hide_observed`: the gateway reads no provider login on its
 * host. `GET /admin/api/session` reports the option, and the dashboard owes it
 * three things — not asking for observations, still showing managed pool usage
 * in the primary table, and not telling the reader to sign in with a provider
 * CLI on a machine that is deliberately not being looked at.
 *
 * Each property is asserted against the option off as well: an assertion about
 * the hidden case alone would also pass against a dashboard that never asked
 * for observations at all.
 */

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function session(hide_observed: boolean) {
  return { csrf: 'test-csrf', expiry_buffer_ms: DEFAULT_EXPIRY_BUFFER_MS, hide_observed };
}

const POOL: PoolProvider[] = [
  {
    provider: 'anthropic',
    auth: 'claude_oauth',
    accounts: [{ name: 'pool-a', has_state: true, utilization_5h: 0.4 } as PoolAccount],
  },
];

const LOCAL_LOGIN_COPY = /Read-only signals from provider clients on this machine/;

describe('a gateway configured with hide_observed', () => {
  it('never asks for observations', async () => {
    const hidden = await renderDashboard({ session: session(true) });
    expect(hidden.calls.map((call) => call.path)).not.toContain('/admin/api/observed');

    cleanup();
    const shown = await renderDashboard({ session: session(false) });
    expect(shown.calls.map((call) => call.path)).toContain('/admin/api/observed');
  });

  it('keeps managed pool usage in the primary table', async () => {
    await renderDashboard({ session: session(true), pool: POOL });

    expect(tbody('observed').getByText('pool-a')).toBeInTheDocument();
    expect(tbody('observed').getByText('5h')).toBeInTheDocument();
  });

  it('does not describe the table as local provider logins', async () => {
    await renderDashboard({ session: session(true) });
    expect(screen.queryByText(LOCAL_LOGIN_COPY)).toBeNull();
    expect(screen.getByText(/Managed pool accounts only/)).toBeInTheDocument();
    expect(tbody('observed').getByText('No managed pool accounts yet.')).toBeInTheDocument();
    expect(tbody('observed').queryByText(/Sign in with a provider CLI/)).toBeNull();

    cleanup();
    await renderDashboard({ session: session(false) });
    expect(screen.getByText(LOCAL_LOGIN_COPY)).toBeInTheDocument();
    expect(tbody('observed').getByText(/Sign in with a provider CLI/)).toBeInTheDocument();
  });

  it('treats a gateway that predates the option as not hiding', async () => {
    const api = await renderDashboard();
    expect(api.calls.map((call) => call.path)).toContain('/admin/api/observed');
    expect(screen.getByText(LOCAL_LOGIN_COPY)).toBeInTheDocument();
  });
});
