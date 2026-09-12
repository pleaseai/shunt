import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { renderDashboard, reply } from '../test/harness';

const CLOSED_NOTICE =
  'the previous authorization step was closed when this start was issued; start again.';

/**
 * A start clears the open authorization step before it sends, by design: a step
 * left open stays clickable and its Complete button posts to the name captured
 * for THAT flow (#513). When the replacement start is then refused, the server
 * still holds the previous pending login for the rest of its `pending_ttl_secs`
 * — but nothing on the page points at it any more, and the operator reads only
 * why the new request failed. The message says what was closed instead (#531).
 */
describe('a refused start says which authorization step it closed', () => {
  it('names the closed step when the refused start had one to close', async () => {
    const user = userEvent.setup();
    let started = 0;
    await renderDashboard(
      {},
      {
        'POST /admin/api/accounts/claude': () => {
          started += 1;
          return started === 1
            ? reply({ name: 'first', authorize_url: 'https://auth.example/claude' })
            : reply({ error: { message: 'name must be lowercase' } }, 400);
        },
      },
    );

    const name = document.getElementById('name') as HTMLInputElement;
    await user.type(name, 'first');
    await user.click(document.getElementById('start') as HTMLButtonElement);
    await screen.findByRole('link', { name: 'https://auth.example/claude' });

    // The operator re-starts under a name the server refuses.
    await user.clear(name);
    await user.type(name, 'Second');
    await user.click(document.getElementById('start') as HTMLButtonElement);

    // The server's own reason is still the lede — the notice is what the page
    // adds about the step that reason cost.
    expect(document.getElementById('addmsg')).toHaveTextContent(
      `name must be lowercase — ${CLOSED_NOTICE}`,
    );
    // And the step really is gone, which is what makes the notice true rather
    // than merely reassuring.
    expect(document.getElementById('step2')).toBeNull();
  });

  it('stays quiet about a closed step when the refused start closed none', async () => {
    const user = userEvent.setup();
    await renderDashboard(
      {},
      {
        'POST /admin/api/accounts/claude': () =>
          reply({ error: { message: 'name must be lowercase' } }, 400),
      },
    );

    await user.type(document.getElementById('name') as HTMLInputElement, 'Second');
    await user.click(document.getElementById('start') as HTMLButtonElement);

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    // Nothing was open, so nothing was lost. Saying it anyway would teach an
    // operator to read past the sentence on the one occasion it is true.
    expect(message).not.toHaveTextContent('previous authorization step');
  });
});
