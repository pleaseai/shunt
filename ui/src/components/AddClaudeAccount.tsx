import { forwardRef, useImperativeHandle, useRef, useState } from 'react';

import { useProvisioningFlow } from '../useProvisioningFlow';

export interface AddAccountHandle {
  /** Point the form at an existing account and clear any half-finished flow. */
  prime: (name: string, kind?: string) => void;
  /** Report a store mutation's outcome in this form's message area. */
  report: (text: string, ok: boolean) => void;
}

type Mode = 'oauth' | 'setup_token';

function modeHelp(mode: Mode): string {
  return mode === 'setup_token'
    ? 'Setup token creates a one-year, inference-only login that cannot refresh.'
    : 'Full OAuth creates a refreshable login that shunt manages.';
}

/**
 * Add — or re-provision — a Claude pool account.
 *
 * Re-login deliberately drives this form rather than a dedicated endpoint:
 * completing the normal provisioning flow under an account's existing name
 * overwrites that account in place, including cleanup when the upstream identity
 * changes (`src/admin/mod.rs`). The login method is preselected from the row's
 * current kind — re-provisioning under the other mode would silently convert the
 * account between refreshable and inference-only.
 */
export const AddClaudeAccount = forwardRef<
  AddAccountHandle,
  { onStored: () => void }
>(function AddClaudeAccount({ onStored }, ref) {
  const [mode, setMode] = useState<Mode>('oauth');
  const nameInput = useRef<HTMLInputElement>(null);
  const flow = useProvisioningFlow({
    endpoints: {
      start: '/accounts/claude',
      complete: (name) => `/accounts/claude/${name}/complete`,
    },
    copy: {
      startFailure: 'Failed to start',
      completeFailure: 'Failed to complete',
      stored: 'Account stored',
    },
    onStored,
  });

  useImperativeHandle(ref, () => ({
    prime(name: string, kind?: string) {
      flow.prime(name);
      setMode(kind === 'setup_token' ? 'setup_token' : 'oauth');
      nameInput.current?.focus({ preventScroll: true });
      nameInput.current?.scrollIntoView?.({ behavior: 'smooth', block: 'center' });
    },
    report: flow.report,
  }), [flow.prime, flow.report]);

  // The authorization step is open exactly while `authorizeUrl` is non-null,
  // and that is what locks the login method: the mode the server's pending
  // entry was created under is fixed at start, so a radio that stayed live
  // would let the form read one method while the pending login is the other.
  const locked = flow.authorizeUrl !== null;

  return (
    <>
      <h2>Add Claude account</h2>
      <div className="card">
        <p id="modehelp" className="muted" style={{ marginTop: 0 }}>
          {modeHelp(mode)}
        </p>
        <label htmlFor="name">
          Account name <span className="muted">(lowercase letters, digits, hyphens)</span>
        </label>
        <input
          id="name"
          name="name"
          ref={nameInput}
          placeholder="e.g. pool-b"
          autoComplete="off"
          spellCheck={false}
          value={flow.name}
          onChange={(event) => flow.setName(event.target.value)}
        />
        {/* The radios are the only controls this form disables on its own, so
            the help text and the lock note are attached here rather than to
            each input: a screen reader entering the group reads why the choice
            is fixed, which a bare `disabled` attribute never says. */}
        <fieldset aria-describedby={locked ? 'modehelp modelock' : 'modehelp'}>
          <legend>Login method</legend>
          <label className="choice">
            <input
              id="mode-oauth"
              type="radio"
              name="mode"
              value="oauth"
              checked={mode === 'oauth'}
              disabled={locked}
              onChange={() => setMode('oauth')}
            />
            <span>Full OAuth (refreshable)</span>
          </label>
          <label className="choice">
            <input
              id="mode-setup"
              type="radio"
              name="mode"
              value="setup_token"
              checked={mode === 'setup_token'}
              disabled={locked}
              onChange={() => setMode('setup_token')}
            />
            <span>Setup token (1-year, inference-only)</span>
          </label>
          {/* `role="status"` makes the lock announce itself the moment it
              appears. The form's own live region (`#addmsg`) cannot carry this:
              `start` clears it and a successful start never sets it, so the one
              moment the radios go dead is the one moment that region is empty. */}
          {locked ? (
            <p id="modelock" role="status" className="muted">
              Locked while the authorization step below is open — the pending login was started
              for this method. Start another login to change it.
            </p>
          ) : null}
        </fieldset>
        <button
          id="start"
          type="button"
          onClick={() => void flow.start({ name: flow.name.trim(), mode })}
        >
          Start account login
        </button>
        {flow.authorizeUrl ? (
          <div id="step2" style={{ marginTop: '1rem' }}>
            <p>1. Open this URL, sign in to the target Claude account, and approve:</p>
            <p className="overflow">
              <a id="authlink" href={flow.authorizeUrl} target="_blank" rel="noopener noreferrer">
                {flow.authorizeUrl}
              </a>
            </p>
            <label htmlFor="code">
              2. Paste the code shown after approval (<code>&lt;code&gt;#&lt;state&gt;</code>)
            </label>
            <textarea
              id="code"
              value={flow.code}
              onChange={(event) => flow.setCode(event.target.value)}
            />
            <div style={{ marginTop: '.6rem' }}>
              <button
                id="complete"
                type="button"
                disabled={flow.completing}
                onClick={() => void flow.complete()}
              >
                Complete
              </button>
            </div>
          </div>
        ) : null}
        <div id="addmsg" aria-live="polite" className={flow.message ? `msg ${flow.message.ok ? 'ok' : 'err'}` : undefined}>
          {flow.message?.text}
        </div>
      </div>
    </>
  );
});
