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
        <fieldset>
          <legend>Login method</legend>
          <label className="choice">
            <input
              id="mode-oauth"
              type="radio"
              name="mode"
              value="oauth"
              checked={mode === 'oauth'}
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
              onChange={() => setMode('setup_token')}
            />
            <span>Setup token (1-year, inference-only)</span>
          </label>
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
