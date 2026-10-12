import { act, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { deferred, renderDashboard, reply, rowOf, tbody, type Route } from '../test/harness';

const CLOSED_NOTICE =
  'the authorization step that was open has been closed; start again.';
const UNANSWERED =
  'No answer from the server — no authorization step opened, so start again';
const UNANSWERED_AFTER_CLOSE =
  'No answer from the server — no authorization step opened, and the one that was open has been closed, so start again';

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
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);
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
    expect(document.getElementById('addmsg')).not.toHaveTextContent(CLOSED_NOTICE);
  });

  /**
   * A step whose code is already submitted is not one the operator could still
   * have completed. Its completion leaves `authorizeUrl` non-null until it
   * succeeds and clears it only after the epoch guard, so a Start clicked
   * mid-completion supersedes that completion — suppressing its confirmation
   * while `onStored` has already stored the account. Claiming a lost step there
   * sends the operator to re-provision an account that is already in the table.
   */
  it('says nothing about a step whose completion was already in flight', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
      },
      'POST /admin/api/accounts/claude/first/complete': (() => held.promise) as Route,
    });

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    // Start is not disabled during a completion, so this click is reachable.
    await user.clear(nameField());
    await user.type(nameField(), 'Second');
    await user.click(startButton());

    // The completion lands last and did store the account.
    await act(async () => {
      held.resolve(reply({ message: 'Account stored' }));
      await held.promise;
    });

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);
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
    expect(message).not.toHaveTextContent(UNANSWERED_AFTER_CLOSE);
  });

  /**
   * The mid-completion exclusion above is a deferral, not a verdict: a start
   * clicked while a completion is in flight cannot yet know whether that
   * exchange stored the account. When it turns out to have failed definitively,
   * nothing was stored and the pending login is still on the server for the rest
   * of its `pending_ttl_secs` — while the step that pointed at it is gone, and
   * the completion's own error is suppressed by the epoch the newer start took.
   * The closure has to reach the operator through the message that does survive.
   */
  it('names the closed step once the completion it superseded has failed', async () => {
    const user = userEvent.setup();
    const completion = deferred<Response>();
    const refusal = deferred<Response>();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': (() => {
        started += 1;
        return started === 1 ? reply(OPENED) : refusal.promise;
      }) as Route,
      'POST /admin/api/accounts/claude/first/complete': (() => completion.promise) as Route,
    });

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    // Start is not disabled during a completion, so this click is reachable.
    await user.clear(nameField());
    await user.type(nameField(), 'Second');
    await user.click(startButton());

    // The completion settles first, and definitively: the exchange was refused,
    // so the pending login it was spending is still live on the server.
    await act(async () => {
      completion.resolve(reply({ error: { message: 'state mismatch' } }, 400));
      await completion.promise;
    });
    // Its own message is suppressed — the newer start owns the epoch.
    expect(document.getElementById('addmsg')).not.toHaveTextContent('state mismatch');

    await act(async () => {
      refusal.resolve(reply(REFUSED, 400));
      await refusal.promise;
    });

    expect(document.getElementById('addmsg')).toHaveTextContent(
      `name must be lowercase — ${CLOSED_NOTICE}`,
    );
  });

  /**
   * Re-login discards the half-finished flow deliberately, at the operator's own
   * request, and clears the page's message with it. A completion that fails
   * after that has no closure to hand to the next start: that start closed
   * nothing, and the notice would say the step went when the start was issued.
   */
  it('stays quiet when a re-login discarded the flow whose completion then failed', async () => {
    const user = userEvent.setup();
    const completion = deferred<Response>();
    let started = 0;
    await renderDashboard(
      { accounts: [{ name: 'other', kind: 'imported' }] },
      {
        'POST /admin/api/accounts/claude': () => {
          started += 1;
          return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
        },
        'POST /admin/api/accounts/claude/first/complete': (() => completion.promise) as Route,
      },
    );

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    await user.click(
      within(rowOf(tbody('accounts').getByText('other'))).getByRole('button', { name: 'Re-login' }),
    );
    await act(async () => {
      completion.resolve(reply({ error: { message: 'state mismatch' } }, 400));
      await completion.promise;
    });

    await user.click(startButton());

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);
  });

  /**
   * The likelier ordering of the pair above: the replacement start's refusal is
   * a local validation and lands first, while the completion is still doing an
   * upstream round trip. By the time that exchange fails there is already a
   * verdict on screen, and handing the closure to the *next* message is not good
   * enough — the next start can as easily succeed, and would clear the fact
   * without anything having said it. The message the operator is reading is
   * amended in place instead.
   */
  it('amends a start failure already on screen when the completion lands after it', async () => {
    const user = userEvent.setup();
    const completion = deferred<Response>();
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
      },
      'POST /admin/api/accounts/claude/first/complete': (() => completion.promise) as Route,
    });

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    await user.clear(nameField());
    await user.type(nameField(), 'Second');
    await user.click(startButton());

    // Nothing is claimed yet: the completion may still store the account.
    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);

    await act(async () => {
      completion.resolve(reply({ error: { message: 'state mismatch' } }, 400));
      await completion.promise;
    });

    expect(message).toHaveTextContent(`name must be lowercase — ${CLOSED_NOTICE}`);
  });

  /**
   * Only the start's own verdict is amended. `#addmsg` is shared with the row
   * actions, which report through `report`, and appending "the previous
   * authorization step that was open has been closed" to a failed
   * refresh names a start that is not what the operator just did. The closure
   * waits for a message that can carry it instead.
   */
  it('leaves another action’s message alone and carries the closure to the next start', async () => {
    const user = userEvent.setup();
    const completion = deferred<Response>();
    let started = 0;
    await renderDashboard(
      { accounts: [{ name: 'other', kind: 'imported' }] },
      {
        'POST /admin/api/accounts/claude': () => {
          started += 1;
          return started === 1 ? reply(OPENED) : reply(REFUSED, 400);
        },
        'POST /admin/api/accounts/claude/first/complete': (() => completion.promise) as Route,
        'POST /admin/api/accounts/claude/other/refresh': () =>
          reply({ error: { message: 'Refresh failed' } }, 500),
      },
    );

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    await user.clear(nameField());
    await user.type(nameField(), 'Second');
    await user.click(startButton());

    // A row action takes over the live region before the completion settles.
    await user.click(
      within(rowOf(tbody('accounts').getByText('other'))).getByRole('button', { name: 'Refresh' }),
    );
    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('Refresh failed');

    await act(async () => {
      completion.resolve(reply({ error: { message: 'state mismatch' } }, 400));
      await completion.promise;
    });
    expect(message).toHaveTextContent('Refresh failed');
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);

    // Held for the next message that can say it.
    await user.click(startButton());
    expect(message).toHaveTextContent(`name must be lowercase — ${CLOSED_NOTICE}`);
  });

  /**
   * A superseded completion that fails has nothing to report when the start that
   * superseded it opened a step of its own: the operator is not stranded.
   * Arming the carry there strands the fact instead — completing that step
   * consumes it silently, and the notice then surfaces on a later failure that
   * closed nothing at all.
   */
  it('does not carry a closure into a flow that has already reopened', async () => {
    const user = userEvent.setup();
    const completion = deferred<Response>();
    const SECOND = { name: 'second', authorize_url: 'https://auth.example/second' };
    let started = 0;
    await renderDashboard({}, {
      'POST /admin/api/accounts/claude': () => {
        started += 1;
        if (started === 1) return reply(OPENED);
        if (started === 2) return reply(SECOND);
        return reply(REFUSED, 400);
      },
      'POST /admin/api/accounts/claude/first/complete': (() => completion.promise) as Route,
      'POST /admin/api/accounts/claude/second/complete': () => reply({ message: 'Account stored' }),
    });

    await user.type(nameField(), 'first');
    await user.click(startButton());
    await screen.findByRole('link', { name: OPENED.authorize_url });

    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    await user.clear(nameField());
    await user.type(nameField(), 'second');
    await user.click(startButton());
    await screen.findByRole('link', { name: SECOND.authorize_url });

    // The stranded completion lands last, and failed.
    await act(async () => {
      completion.resolve(reply({ error: { message: 'state mismatch' } }, 400));
      await completion.promise;
    });

    // The reopened flow completes normally, leaving an empty form.
    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'second#state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);
    await screen.findByText('Account stored');

    await user.type(nameField(), 'Third');
    await user.click(startButton());

    const message = document.getElementById('addmsg');
    expect(message).toHaveTextContent('name must be lowercase');
    expect(message).not.toHaveTextContent(CLOSED_NOTICE);
  });
});
