import { forwardRef, useImperativeHandle, useRef } from 'react';

import { useProvisioningFlow } from '../useProvisioningFlow';
import type { AddAccountHandle } from './AddClaudeAccount';

/**
 * Add — or re-provision — a Codex pool account.
 *
 * The Codex counterpart of the Claude form, and safe for the same reason:
 * `POST /admin/api/accounts/codex` has no duplicate-name guard, and completion
 * captures the pre-store identity, overwrites the account in place, and hands
 * both identities to `cleanup_reprovisioned_pool_health` (`src/admin/mod.rs`).
 * There is no login method to preselect here — ChatGPT OAuth is the only way
 * into this store.
 */
export const AddCodexAccount = forwardRef<AddAccountHandle, { onStored: () => void }>(
  function AddCodexAccount({ onStored }, ref) {
    const nameInput = useRef<HTMLInputElement>(null);
    const flow = useProvisioningFlow({
      endpoints: {
        start: '/accounts/codex',
        complete: (name) => `/accounts/codex/${name}/complete`,
      },
      copy: {
        startFailure: 'Failed to start Codex login',
        completeFailure: 'Failed to complete Codex login',
        stored: 'Codex account stored',
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
        <h2>Add Codex account</h2>
        <div className="card">
          <p className="muted" style={{ marginTop: 0 }}>
            ChatGPT OAuth creates a refreshable login that shunt manages.
          </p>
          <label htmlFor="codex-name">
            Account name <span className="muted">(lowercase letters, digits, hyphens)</span>
          </label>
          <input
            id="codex-name"
            name="codex-name"
            ref={nameInput}
            placeholder="e.g. codex-backup"
            autoComplete="off"
            spellCheck={false}
            value={flow.name}
            onChange={(event) => flow.setName(event.target.value)}
          />
          <button
            id="start-codex"
            type="button"
            style={{ marginTop: '.7rem' }}
            onClick={() => void flow.start({ name: flow.name.trim() })}
          >
            Start Codex login
          </button>
          {flow.authorizeUrl ? (
            <div id="codex-step2" style={{ marginTop: '1rem' }}>
              <p>1. Open this URL, sign in to the target ChatGPT account, and approve:</p>
              <p className="overflow">
                <a
                  id="codex-authlink"
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
              <label htmlFor="codex-code">
                2. Paste the full redirected URL from the browser address bar
              </label>
              <textarea
                id="codex-code"
                name="codex-code"
                spellCheck={false}
                placeholder="http://localhost:1455/auth/callback?code=…&state=…"
                value={flow.code}
                onChange={(event) => flow.setCode(event.target.value)}
              />
              <div style={{ marginTop: '.6rem' }}>
                <button
                  id="complete-codex"
                  type="button"
                  disabled={flow.completing}
                  onClick={() => void flow.complete()}
                >
                  Complete Codex login
                </button>
              </div>
            </div>
          ) : null}
          <div
            id="codex-addmsg"
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
