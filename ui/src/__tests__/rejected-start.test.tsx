import { act, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { deferred, renderDashboard, reply, type Route } from '../test/harness';

const CLOSED_NOTICE =
  'the previous authorization step was closed when this start was issued; start again.';
const UNANSWERED =
  'No answer from the server — no authorization step opened, so start again';
const UNANSWERED_AFTER_CLOSE =
  'No answer from the server — no authorization step opened, and the previous one was closed when this start was issued, so start again';

const OPENED = { name: 'first', authorize_url: 'https://auth.example/claude' };
const REFUSED = { error: { message: 'name must be lowercase' } };

function nameField(): HTMLInputElement {
  return document.getElementById('name') as HTMLInputElement;
}
function startButton(): HTMLButtonElement {
  return document.getElementById('start') as HTMLButtonElement;
}

/** Open an authorization step, then point the form at a name the server refuses. */
async function openThenRetarget(user: ReturnType<typeof userEvent.setup>): Promise<void> {
  await user.type(nameField(), 'first');
  await user.click(startButton());
  await screen.findByRole('link', { name: OPENED.authorize_url });
  await user.clear(nameField());
  await user.type(nameField(), 'Second');
}

/**
 * A start clears the open authorization step before it sends, by design: a step
 * left open stays clickable and its Complete button posts to the name captured
 * for THAT flow (#513). When that start then fails, the server still holds the
 * previous pending login for the rest of its `pending_ttl_secs` — but nothing on
 * the page points at it any more, and the operator reads only why the new
 * request failed. The message says what was closed instead (#531).
 */
describe('a failed start says which authorization step it closed', () => {
  it('names the closed step when the refused start had one to close', async () => {
    const user = userEvent.setup();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
      },
    });

    await openThenRetarget(user);
    await user.click(startButton());

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
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => reply(REFUSED, 400),
    });

    await user.type(nameField(), 'Second');
    await user.click(startButton());

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    // Nothing was open, so nothing was lost. Saying it anyway would teach an
    // operator to read past the sentence on the one occasion it is true.
    expect(message).not.toHaveTextContent('previous authorization step');
  });

  /**
   * The closure and the message that reports it can be separated. Start A closes
   * the step, start B supersedes A before it lands, so the epoch guard drops A's
   * own response — and B reads an `authorizeUrl` that A already nulled. Without
   * the ref that carries the closure across that gap, the step A closed is
   * announced to nobody. The Start button is not disabled while a start is in
   * flight (unlike Complete), so this is an ordinary double click, not a race.
   */
  it('reports a closed step that a superseded start never got to report', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': (() => {
        started += 1;
        if (started === 1) return reply(OPENED);
        if (started === 2) return held.promise;
        return reply(REFUSED, 400);
      }) as Route,
    });

    await openThenRetarget(user);
    await user.click(startButton()); // A — closes the step, left in flight
    await user.click(startButton()); // B — supersedes A, and is refused

    expect(document.getElementById('addmsg')).toHaveTextContent(
      `name must be lowercase — ${CLOSED_NOTICE}`,
    );

    // A lands last. The epoch guard owns its silence; assert it neither reopens
    // a step nor overwrites the verdict the operator is reading.
    await act(async () => {
      held.resolve(reply({ name: 'Second', authorize_url: 'https://auth.example/late' }));
      await held.promise;
    });
    expect(screen.queryByRole('link', { name: 'https://auth.example/late' })).toBeNull();
    expect(document.getElementById('addmsg')).toHaveTextContent(
      `name must be lowercase — ${CLOSED_NOTICE}`,
    );
  });

  /**
   * Once a message has reported the closure, the fact is spent. A later start
   * that closes nothing must not inherit it — the ref is what would carry it
   * forward, so it has to be released by whichever message reports it.
   */
  it('does not repeat the notice on a later start that closed nothing', async () => {
    const user = userEvent.setup();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
      },
    });

    await openThenRetarget(user);
    await user.click(startButton());
    expect(document.getElementById('addmsg')).toHaveTextContent(CLOSED_NOTICE);

    // Nothing is open now, and the closure has already been reported.
    await user.click(startButton());
    expect(document.getElementById('addmsg')).not.toHaveTextContent('previous authorization step');
  });

  /**
   * The clear runs before the request, so an unanswered start has closed the
   * step just as surely as a refused one. Its own text says only that no NEW
   * step opened.
   */
  it('names the closed step when the start got no answer at all', async () => {
    const user = userEvent.setup();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        if (started === 1) return reply(OPENED);
        throw new TypeError('Failed to fetch');
      },
    });

    await openThenRetarget(user);
    await user.click(startButton());

    expect(document.getElementById('addmsg')).toHaveTextContent(UNANSWERED_AFTER_CLOSE);
  });

  it('says only that no answer came when the unanswered start closed nothing', async () => {
    const user = userEvent.setup();
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        throw new TypeError('Failed to fetch');
      },
    });

    await user.type(nameField(), 'first');
    await user.click(startButton());

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent(UNANSWERED);
    expect(message).not.toHaveTextContent('the previous one was closed');
  });
});
