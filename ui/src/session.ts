import { createContext, useContext } from 'react';

export interface Session {
  /** The session's CSRF token; empty for a header-credential caller. */
  csrf: string;
  /** `claude::auth::EXPIRY_BUFFER` in milliseconds, as the server reports it. */
  expiryBufferMs: number;
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
