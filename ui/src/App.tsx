import { useEffect, useState, type ReactElement } from 'react';

import { API, readJson } from './api';
import { Dashboard } from './Dashboard';
import { SessionProvider, type Session } from './session';
import type { SessionBootstrap } from './types';

/**
 * The bundle's entry point: fetch the two per-session values the dashboard
 * cannot be built with, then render it.
 *
 * The shell that loads this bundle is one static file embedded at compile time
 * and served without a credential, identical for every visitor, so — unlike the
 * server-rendered page it replaces — it cannot have the CSRF token and the
 * refresh buffer interpolated into it. `GET /admin/api/session` supplies both
 * over the same cookie the rest of the surface authenticates.
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
