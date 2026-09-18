import { useEffect, useState, type ReactElement } from 'react';

import { API, readJson } from './api';
import { Dashboard } from './Dashboard';
import { SessionProvider, type Session } from './session';
import type { SessionBootstrap } from './types';

/**
 * The bundle's entry point: fetch the per-session values the dashboard
 * cannot be built with, then render it.
 *
 * The shell that loads this bundle is one static file embedded at compile time
 * and served without a credential, identical for every visitor, so — unlike the
 * server-rendered page it replaces — it cannot have them interpolated into it.
 * `GET /admin/api/session` supplies them over the same cookie the rest of
 * the surface authenticates:
 *
 * - `csrf` — the synchronizer token every cookie-authenticated mutation sends
 *   back as `x-csrf-token`. Empty for a header-credential caller, which is
 *   CSRF-exempt for having no ambient cookie.
 * - `access` — the tier the credential that minted this session carried. A
 *   `[server.admin] read_keys` login mints a read-tier session, so a cookie no
 *   longer implies write; `useCanWrite` is the one place it is interpreted.
 * - `expiry_buffer_ms` — `claude::auth::EXPIRY_BUFFER`, served rather than
 *   copied so the TypeScript cannot drift from the Rust constant.
 * - `hide_observed` — `[server.admin] hide_observed`. The gateway reads no
 *   provider login on its host, so the dashboard skips the observation read.
 */
export function App(): ReactElement {
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      const result = await readJson<SessionBootstrap>(
        `${API}/session`,
        'Could not start an admin session',
      );
      if (!result.ok) {
        // The shell is served to anyone; the data behind it is not. An
        // unauthenticated load lands here, and the sign-in page is where it
        // belongs — the server-rendered `/admin` redirects for the same reason.
        if (result.status === 401) {
          window.location.assign('/admin/login');
          return;
        }
        setError(result.message);
        return;
      }
      setSession({
        csrf: result.data.csrf,
        expiryBufferMs: result.data.expiry_buffer_ms,
        // `readJson` casts the response body rather than validating it, so
        // `access` is asserted to be an `AdminAccess`, not checked. Defaulting
        // an absent field to the *lower* tier is what makes that safe: the page
        // then hides its write affordances rather than unlocking them. An
        // unrecognized value needs no default — `useCanWrite` compares against
        // `'write'`, so anything else already reads as read-only.
        access: result.data.access ?? 'read',
        hideObserved: result.data.hide_observed === true,
      });
    })();
  }, []);

  if (error) {
    return (
      <main>
        <h1>shunt admin</h1>
        <div className="msg err">{error}</div>
      </main>
    );
  }

  if (!session) {
    return (
      <main>
        <h1>shunt admin</h1>
        <p className="muted">Loading…</p>
      </main>
    );
  }

  return (
    <SessionProvider value={session}>
      <Dashboard />
    </SessionProvider>
  );
}
