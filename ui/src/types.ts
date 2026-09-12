/** The JSON shapes `/admin/api/*` returns, named as the Rust handlers spell them. */

/** `config::AdminAccess`, as the admin API spells it. `write` implies `read`. */
export type AdminAccess = 'read' | 'write';

export interface SessionBootstrap {
  /** The session's CSRF token; empty for a header-credential caller. */
  csrf: string;
  /**
   * The privilege this session authenticates with — what the dashboard renders
   * its write affordances from. A `[server.admin] read_keys` login mints a
   * `read` session, which every mutation route refuses with `403`.
   */
  access: AdminAccess;
  /**
   * `claude::auth::EXPIRY_BUFFER` in milliseconds. Served rather than copied
   * here so this bundle and routing cannot disagree about when a setup token
   * stops being usable.
   */
  expiry_buffer_ms: number;
}

export interface QuotaBucket {
  label: string;
  remaining: number | null;
  reset_time: string | null;
}

export interface ObservedAccount {
  provider: string;
  source: string;
  identity: string | null;
  detail: string | null;
  state: string;
  signal?: string | null;
  message?: string | null;
  uuid?: string | null;
  quota_buckets?: QuotaBucket[];
  utilization_5h?: number | null;
  reset_5h?: number | null;
  utilization_7d?: number | null;
  reset_7d?: number | null;
  utilization_7d_oi?: number | null;
  reset_7d_oi?: number | null;
}

export interface PoolAccount {
  name: string;
  plan?: string | null;
  status?: string | null;
  disabled?: boolean;
  needs_relogin?: boolean;
  has_state?: boolean;
  near_quota?: boolean;
  cooldown_secs_remaining?: number | null;
  cooldown_fable_secs_remaining?: number | null;
  utilization_5h?: number | null;
  reset_5h?: number | null;
  utilization_7d?: number | null;
  reset_7d?: number | null;
  utilization_7d_oi?: number | null;
  reset_7d_oi?: number | null;
}

export interface PoolProvider {
  provider: string;
  /** The provider's configured auth kind, e.g. `claude_oauth`. */
  auth?: string | null;
  accounts?: PoolAccount[];
}

export interface ClaudeStoreAccount {
  name: string;
  kind: string;
  expires_at?: number | null;
  uuid?: string | null;
}

export interface CodexStoreAccount {
  name: string;
  expires_at?: number | null;
  account_id?: string | null;
}

export interface StatusSource {
  provider: string;
  indicator: string;
  description?: string | null;
  error?: string | null;
  observed_at?: number | null;
}

/**
 * One line of the grouped "Accounts and usage" table: a managed pool account, a
 * read-only observation, or the same subscription seen through both.
 */
export interface AccountRow {
  provider: string;
  label: string;
  detail: string | null;
  managed: PoolAccount | null;
  observed: ObservedAccount | null;
  state: string;
  utilization_5h?: number | null;
  reset_5h?: number | null;
  utilization_7d?: number | null;
  reset_7d?: number | null;
  utilization_7d_oi?: number | null;
  reset_7d_oi?: number | null;
}
