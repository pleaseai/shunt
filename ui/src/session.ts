import { createContext, useContext } from 'react';

import type { AdminAccess } from './types';

export interface Session {
  /** The session's CSRF token; empty for a header-credential caller. */
  csrf: string;
  /** `claude::auth::EXPIRY_BUFFER` in milliseconds, as the server reports it. */
  expiryBufferMs: number;
  /** The privilege this session authenticates with. */
  access: AdminAccess;
}

const SessionContext = createContext<Session | null>(null);

export const SessionProvider = SessionContext.Provider;

/**
 * Throws rather than defaulting: a zero refresh buffer would silently report a
 * setup token as usable for the last five minutes of its life, which is the one
 * window the warning exists for.
 */
export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error('useSession outside a SessionProvider');
  return session;
}

/**
 * Whether this session may mutate — the single place the tier is interpreted,
 * so a new write affordance cannot read it a slightly different way.
 *
 * This hides affordances; it does not protect anything. `require_write` on the
 * server is the enforcement, and a read session that reached a mutation anyway
 * gets a `403`. Comparing against `'write'` rather than against `'read'` is
 * deliberate: an unrecognized value then reads as read-only rather than
 * unlocking the page.
 */
export function useCanWrite(): boolean {
  return useSession().access === 'write';
}
