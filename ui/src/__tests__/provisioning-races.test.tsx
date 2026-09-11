import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';

import { deferred, renderDashboard, reply, rowOf, tbody, type Route } from '../test/harness';

const ACCOUNTS = {
  accounts: [{ name: 'other-claude', kind: 'imported' }],
  codexAccounts: [{ name: 'other-codex', account_id: 'acct-1' }],
};

function reloginClaude(name: string): HTMLElement {
  return within(rowOf(tbody('accounts').getByText(name))).getByRole('button', { name: 'Re-login' });
}
function reloginCodex(name: string): HTMLElement {
  return within(rowOf(tbody('codex-accounts').getByText(name))).getByRole('button', {
    name: 'Re-login',
  });
}

/** Start a Claude flow whose authorization step is already open. */
async function openClaudeFlow(user: ReturnType<typeof userEvent.setup>, name: string): Promise<void> {
  await user.type(document.getElementById('name') as HTMLInputElement, name);
  await user.click(document.getElementById('start') as HTMLButtonElement);
  await screen.findByRole('link', { name: 'https://auth.example/claude' });
}

describe('a superseded provisioning response cannot restore a cleared flow', () => {
  /**
   * A start still in flight when the form is re-primed would reopen the previous
   * account's authorization step while the name field already reads the newly
   * picked account. Following that reopened link stores the freshly authorized
   * credential under the OLD account's name — silently overwriting a different
   * pool account.
   */
  it('discards a start response that a re-login has already superseded', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() => held.promise) as Route,
    });

    await user.type(document.getElementById('name') as HTMLInputElement, 'first');
    await user.click(document.getElementById('start') as HTMLButtonElement);

    // The operator changes their mind and re-primes the form for another account.
    await user.click(reloginClaude('other-claude'));
    expect((document.getElementById('name') as HTMLInputElement).value).toBe('other-claude');

    // The abandoned start now lands.
    await act(async () => {
      held.resolve(reply({ name: 'first', authorize_url: 'https://auth.example/claude' }));
      await held.promise;
    });

    // It must not reopen a flow for the account that is no longer named here.
    expect(document.getElementById('step2')).toBeNull();
    expect(screen.queryByRole('link', { name: 'https://auth.example/claude' })).toBeNull();
    expect((document.getElementById('name') as HTMLInputElement).value).toBe('other-claude');
    expect(document.getElementById('addmsg')).toHaveTextContent('');
  });

  /** The same guard on the failure path: a superseded flow reports nothing. */
  it('stays silent when a superseded start fails', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() => held.promise) as Route,
    });

    await user.type(document.getElementById('name') as HTMLInputElement, 'first');
    await user.click(document.getElementById('start') as HTMLButtonElement);
    await user.click(reloginClaude('other-claude'));

    await act(async () => {
      held.resolve(reply({ error: { message: 'upstream refused' } }, 400));
      await held.promise;
    });

    expect(document.getElementById('addmsg')).not.toHaveTextContent('upstream refused');
  });

  /**
   * A completion that reached the server stored the account whether or not the
   * flow was superseded, so the tables must refresh either way. Only the
   * confirmation and the form reset are gated — those would stomp the newly
   * primed flow.
   */
  it('refreshes the tables but not the form when a completion is superseded', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    const api = await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': () =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' }),
      'POST /admin/api/accounts/claude/first/complete': (() => held.promise) as Route,
    });

    await openClaudeFlow(user, 'first');
    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');
    await user.click(document.getElementById('complete') as HTMLButtonElement);

    await user.click(reloginClaude('other-claude'));
    const before = api.callsTo('GET', '/admin/api/observed').length;

    await act(async () => {
      held.resolve(reply({ message: 'Account stored' }));
      await held.promise;
    });

    // The store write happened, so the grouped view must be re-read...
    await waitFor(() =>
      expect(api.callsTo('GET', '/admin/api/observed').length).toBeGreaterThan(before),
    );
    // ...but the confirmation would speak for a flow the operator abandoned.
    expect(document.getElementById('addmsg')).not.toHaveTextContent('Account stored');
    expect((document.getElementById('name') as HTMLInputElement).value).toBe('other-claude');
  });

  /**
   * The two forms count separately. Re-priming one while the other has a start
   * in flight must not discard that other flow — a shared counter would.
   */
  it('keeps each form’s flow independent of the other’s', async () => {
    const user = userEvent.setup();
    const heldCodex = deferred<Response>();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/codex': (() => heldCodex.promise) as Route,
    });

    await user.type(document.getElementById('codex-name') as HTMLInputElement, 'codex-first');
    await user.click(document.getElementById('start-codex') as HTMLButtonElement);

    // Re-prime the *Claude* form while the Codex start is still in flight.
    await user.click(reloginClaude('other-claude'));

    await act(async () => {
      heldCodex.resolve(reply({ name: 'codex-first', authorize_url: 'https://auth.example/codex' }));
      await heldCodex.promise;
    });

    // The Codex flow was never superseded, so its authorization step opens.
    expect(await screen.findByRole('link', { name: 'https://auth.example/codex' })).toBeInTheDocument();
  });

  /**
   * A completion is the one request in this flow that consumes the pending
   * login. Letting a second click bump the epoch would silence the successful
   * response and surface the failed one instead — reporting an error over an
   * account that was in fact stored, and leaving the finished form open.
   */
  it('refuses a second completion click rather than letting it supersede the first', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    const api = await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': () =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' }),
      'POST /admin/api/accounts/claude/first/complete': (() => held.promise) as Route,
    });

    await openClaudeFlow(user, 'first');
    await user.type(document.getElementById('code') as HTMLTextAreaElement, 'the-code#the-state');

    const complete = document.getElementById('complete') as HTMLButtonElement;
    // Two clicks in ONE task, which is the window the marker exists for: React
    // has not re-rendered between them, so the `disabled` attribute the first
    // click sets is not on the button yet when the second handler runs. Driving
    // this with two awaited `user.click`s instead would only prove the attribute
    // works and would leave the marker itself untested.
    await act(async () => {
      complete.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      complete.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    expect(api.callsTo('POST', '/admin/api/accounts/claude/first/complete')).toHaveLength(1);
    // The attribute is the affordance that keeps a third click from even trying.
    expect(complete).toBeDisabled();
    await user.click(complete);
    expect(api.callsTo('POST', '/admin/api/accounts/claude/first/complete')).toHaveLength(1);

    await act(async () => {
      held.resolve(reply({ message: 'Account stored' }));
      await held.promise;
    });

    // The one completion that ran is the one whose verdict the operator reads.
    await waitFor(() => expect(document.getElementById('addmsg')).toHaveTextContent('Account stored'));
    expect(document.getElementById('addmsg')).toHaveClass('msg', 'ok');
    expect(document.getElementById('step2')).toBeNull();
    expect((document.getElementById('name') as HTMLInputElement).value).toBe('');
  });

  /**
   * `start` sends the selected mode and the server's pending entry is fixed from
   * that moment, so radios that stay live while the authorization step is open
   * let the form read "Setup token" over a pending OAuth login.
   */
  it('closes the login-method radios while an authorization step is open', async () => {
    const user = userEvent.setup();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': () =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' }),
    });

    const oauth = document.getElementById('mode-oauth') as HTMLInputElement;
    const setup = document.getElementById('mode-setup') as HTMLInputElement;
    expect(oauth).toBeEnabled();
    expect(setup).toBeEnabled();

    await openClaudeFlow(user, 'first');

    expect(oauth).toBeDisabled();
    expect(setup).toBeDisabled();
  });

  /**
   * A `disabled` attribute states no reason, and the form's own live region is
   * empty at exactly this moment — `start` clears it and a successful start
   * never sets one. The lock announces and names itself instead.
   */
  it('names the reason the login-method radios are locked', async () => {
    const user = userEvent.setup();
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': () =>
        reply({ name: 'first', authorize_url: 'https://auth.example/claude' }),
    });

    const group = document.getElementById('mode-oauth')!.closest('fieldset')!;
    expect(document.getElementById('modelock')).toBeNull();
    expect(group.getAttribute('aria-describedby')).toBe('modehelp');

    await openClaudeFlow(user, 'first');

    const note = document.getElementById('modelock')!;
    expect(note).toHaveAttribute('role', 'status');
    expect(note.textContent).toMatch(/authorization step/i);
    expect(group.getAttribute('aria-describedby')).toBe('modehelp modelock');
  });

  /**
   * A second start leaves the first authorization step on screen while it is in
   * flight, and its Complete button posts to the name captured for the previous
   * flow. The step is closed the moment the new start is issued instead.
   */
  it('clears the open authorization step as soon as a second start is issued', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    let starts = 0;
    await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/claude': (() => {
        starts += 1;
        return starts === 1
          ? reply({ name: 'first', authorize_url: 'https://auth.example/claude' })
          : held.promise;
      }) as Route,
    });

    await openClaudeFlow(user, 'first');

    // The operator renames the account and starts again; the second start is
    // still in flight.
    await user.clear(document.getElementById('name') as HTMLInputElement);
    await user.type(document.getElementById('name') as HTMLInputElement, 'second');
    await user.click(document.getElementById('start') as HTMLButtonElement);

    expect(document.getElementById('step2')).toBeNull();
    expect(screen.queryByRole('link', { name: 'https://auth.example/claude' })).toBeNull();

    // The new flow's own step opens when its start lands.
    await act(async () => {
      held.resolve(reply({ name: 'second', authorize_url: 'https://auth.example/second' }));
      await held.promise;
    });
    expect(
      await screen.findByRole('link', { name: 'https://auth.example/second' }),
    ).toBeInTheDocument();
  });

  /** The Codex form carries the same two guards, on its own counter. */
  it('applies both guards to the Codex flow as well', async () => {
    const user = userEvent.setup();
    const held = deferred<Response>();
    const api = await renderDashboard(ACCOUNTS, {
      'POST /admin/api/accounts/codex': () =>
        reply({ name: 'first', authorize_url: 'https://auth.example/codex' }),
      'POST /admin/api/accounts/codex/first/complete': (() => held.promise) as Route,
    });

    await user.type(document.getElementById('codex-name') as HTMLInputElement, 'first');
    await user.click(document.getElementById('start-codex') as HTMLButtonElement);
    await screen.findByRole('link', { name: 'https://auth.example/codex' });
    await user.type(
      document.getElementById('codex-code') as HTMLTextAreaElement,
      'http://localhost:1455/auth/callback?code=abc&state=s',
    );

    const complete = document.getElementById('complete-codex') as HTMLButtonElement;
    await act(async () => {
      complete.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      complete.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    expect(api.callsTo('POST', '/admin/api/accounts/codex/first/complete')).toHaveLength(1);
    expect(complete).toBeDisabled();

    // Supersede the in-flight completion, then let it land.
    await user.click(reloginCodex('other-codex'));
    const before = api.callsTo('GET', '/admin/api/observed').length;
    await act(async () => {
      held.resolve(reply({ message: 'Codex account stored' }));
      await held.promise;
    });

    await waitFor(() =>
      expect(api.callsTo('GET', '/admin/api/observed').length).toBeGreaterThan(before),
    );
    expect(document.getElementById('codex-addmsg')).not.toHaveTextContent('Codex account stored');
    expect((document.getElementById('codex-name') as HTMLInputElement).value).toBe('other-codex');
  });
});
