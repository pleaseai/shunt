import { RouterProvider } from '@tanstack/react-router';
import { useEffect, useState, type ReactElement } from 'react';

import { API, readJson } from './api';
import { createAdminRouter } from './router';
import { SessionProvider, type Session } from './session';
import type { SessionBootstrap } from './types';

export function App(): ReactElement {
  const [session, setSession] = useState<Session | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [router] = useState(createAdminRouter);

  useEffect(() => {
    void (async () => {
      const result = await readJson<SessionBootstrap>(
        `${API}/session`,
        'Could not start an admin session',
      );
      if (!result.ok) {
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
        access: result.data.access ?? 'read',
      });
    })();
  }, []);

  if (error) {
    return (
      <main className="mx-auto max-w-[68rem] px-5 py-8">
        <h1 className="text-[1.35rem] tracking-[-0.04em]">shunt admin</h1>
        <div className="msg err">{error}</div>
      </main>
    );
  }

  if (!session) {
    return (
      <main className="mx-auto max-w-[68rem] px-5 py-8">
        <h1 className="text-[1.35rem] tracking-[-0.04em]">shunt admin</h1>
        <p className="muted">Loading…</p>
      </main>
    );
  }

  return (
    <SessionProvider value={session}>
      <RouterProvider router={router} />
    </SessionProvider>
  );
}
