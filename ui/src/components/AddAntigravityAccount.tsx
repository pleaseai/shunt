import { forwardRef, useImperativeHandle, useRef } from 'react';

import { useProvisioningFlow } from '../useProvisioningFlow';
import type { AddAccountHandle } from './AddClaudeAccount';

/**
 * Add — or re-provision — an Antigravity pool account.
 *
 * The Antigravity counterpart of the Codex form: `POST
 * /admin/api/accounts/antigravity` has no duplicate-name guard, and
 * completion overwrites the account in place. Antigravity's registered OAuth
 * redirect is a fixed loopback port (51121) the admin server does not listen
 * on, the same shape as Codex's — so the operator copies the failed
 * redirect's URL from the browser address bar, same as there. There is no
 * login method to preselect — Antigravity OAuth is the only way into this
 * store.
 */
export const AddAntigravityAccount = forwardRef<AddAccountHandle, { onStored: () => void }>(
  function AddAntigravityAccount({ onStored }, ref) {
    const nameInput = useRef<HTMLInputElement>(null);
    const flow = useProvisioningFlow({
      endpoints: {
        start: '/accounts/antigravity',
        complete: (name) => `/accounts/antigravity/${name}/complete`,
      },
      copy: {
        startFailure: 'Failed to start Antigravity login',
        completeFailure: 'Failed to complete Antigravity login',
        stored: 'Antigravity account stored',
      },
      onStored,
    });

    useImperativeHandle(ref, () => ({
      prime(name: string) {
        flow.prime(name);
        nameInput.current?.focus({ preventScroll: true });
        nameInput.current?.scrollIntoView?.({ behavior: 'smooth', block: 'center' });
      },
      report: flow.report,
    }), [flow.prime, flow.report]);

    return (
      <>
        <h2>Add Antigravity account</h2>
        <div className="card">
          <p className="muted mt-0">
            Antigravity OAuth creates a refreshable login that shunt manages.
          </p>
          <label htmlFor="antigravity-name">
            Account name <span className="muted">(lowercase letters, digits, hyphens)</span>
          </label>
          <input
            id="antigravity-name"
            name="antigravity-name"
            ref={nameInput}
            placeholder="e.g. antigravity-backup"
            autoComplete="off"
            spellCheck={false}
            value={flow.name}
            onChange={(event) => flow.setName(event.target.value)}
          />
          <button
            id="start-antigravity"
            type="button"
            className="mt-3"
            onClick={() => void flow.start({ name: flow.name.trim() })}
          >
            Start Antigravity login
          </button>
          {flow.authorizeUrl ? (
            <div id="antigravity-step2" className="mt-4">
              <p>1. Open this URL, sign in to the target Google account, and approve:</p>
              <p className="overflow">
                <a
                  id="antigravity-authlink"
                  href={flow.authorizeUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  {flow.authorizeUrl}
                </a>
              </p>
              <p className="muted">
                The localhost callback page will fail to load. This is expected; copy the full URL
                from the browser address bar.
              </p>
              <label htmlFor="antigravity-code">
                2. Paste the full redirected URL from the browser address bar
              </label>
              <textarea
                id="antigravity-code"
                name="antigravity-code"
                spellCheck={false}
                placeholder="http://localhost:51121/oauth-callback?code=…&state=…"
                value={flow.code}
                onChange={(event) => flow.setCode(event.target.value)}
              />
              <div className="mt-2.5">
                <button
                  id="complete-antigravity"
                  type="button"
                  disabled={flow.completing}
                  onClick={() => void flow.complete()}
                >
                  Complete Antigravity login
                </button>
              </div>
            </div>
          ) : null}
          <div
            id="antigravity-addmsg"
            aria-live="polite"
            className={flow.message ? `msg ${flow.message.ok ? 'ok' : 'err'}` : undefined}
          >
            {flow.message?.text}
          </div>
        </div>
      </>
    );
  },
);
