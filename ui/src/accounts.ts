import { titleCase, when } from './format';
import type {
  AccountRow,
  ClaudeStoreAccount,
  ObservedAccount,
  PoolProvider,
} from './types';

/**
 * A Claude store row's status, derived from its credential kind and never from
 * the raw `expires_at` alone.
 *
 * That timestamp is the ~8h ACCESS-token deadline and means opposite things per
 * kind: an `imported` account carries a refresh token and shunt renews it
 * in-band (`src/auth/claude/auth.rs`), so a past timestamp there is routine and
 * needs no operator action, while a `setup_token` account has no refresh token
 * at all (one-year lifetime), so a past timestamp is a dead credential only a
 * re-login can fix. Rendering the same raw timestamp for both is what made
 * healthy imported accounts read as expired; the raw value is kept as a tooltip
 * instead.
 *
 * A setup token stops being usable `expiryBufferMs` before its own `expiresAt`,
 * not at it: `Tokens::is_valid_at` accepts a credential only while
 * `expiresAt > now + EXPIRY_BUFFER`, and a setup token has no refresh token, so
 * inside that window a routed request already fails on the no-refresh-token
 * path. Reporting it usable there would have the dashboard contradict routing
 * for the last five minutes of the credential's life, which is precisely the
 * window an operator needs the warning in. `expiryBufferMs` is the value
 * `GET /admin/api/session` serves from that same Rust constant rather than a
 * copy, so the two cannot drift apart.
 */
export function accountStatus(
  kind: string,
  expiresAt: number | null | undefined,
  expiryBufferMs: number,
): { state: string; text: string; note: string } {
  if (kind === 'imported') {
    return {
      state: 'available',
      text: 'Auto-refreshes',
      note: 'shunt renews this login as needed',
    };
  }
  if (expiresAt && expiresAt > Date.now() + expiryBufferMs) {
    return {
      state: 'available',
      text: `Valid until ${when(expiresAt)}`,
      note: 'Setup token · re-login before this date',
    };
  }
  return {
    state: 'expired',
    text: 'Expired',
    note: 'Setup token cannot refresh · re-login required',
  };
}

/**
 * Pool providers are config-named (`[providers.anthropic]`); observations are
 * vendor-named. Only the built-in kind is mapped — a provider table under any
 * other name simply renders as its own group rather than being mis-merged.
 */
const POOL_PROVIDER_ALIASES: Record<string, string> = { anthropic: 'claude' };

/** The window fields an observation may override on a coalesced row. */
const WINDOW_KEYS = [
  'utilization_5h',
  'reset_5h',
  'utilization_7d',
  'reset_7d',
  'utilization_7d_oi',
  'reset_7d_oi',
] as const;

/**
 * Fold managed pool accounts and read-only observations into one row set per
 * provider.
 *
 * Coalesced by account uuid: when a managed account holds the same subscription
 * as the local client login, that is ONE account seen through two lenses, not
 * two accounts. Listing it twice is precisely what made this table unreadable
 * with a pool configured.
 */
export function accountGroups(
  observed: ObservedAccount[],
  pool: { providers?: PoolProvider[] } | null,
  accounts: { accounts?: ClaudeStoreAccount[] } | null,
): Map<string, AccountRow[]> {
  const uuidByName: Record<string, string> = {};
  for (const account of accounts?.accounts ?? []) {
    if (account.uuid) uuidByName[account.name] = account.uuid;
  }

  const groups = new Map<string, AccountRow[]>();
  const groupFor = (provider: string): AccountRow[] => {
    const existing = groups.get(provider);
    if (existing) return existing;
    const created: AccountRow[] = [];
    groups.set(provider, created);
    return created;
  };
  const byUuid = new Map<string, AccountRow>();

  for (const table of pool?.providers ?? []) {
    const provider = POOL_PROVIDER_ALIASES[table.provider] ?? table.provider;
    for (const account of table.accounts ?? []) {
      const row: AccountRow = {
        provider,
        label: account.name,
        // A managed-only row (no observed override below) shows the pool's own
        // plan as its detail caption, matching the wording the observed path
        // already uses ("Max plan") — see `observation::parse_claude`.
        detail: account.plan ? `${titleCase(account.plan)} plan` : null,
        managed: account,
        observed: null,
        // `needs_relogin` is checked before the cooldown states for the same
        // reason the pool table checks it first: a dead credential is *also*
        // cooling, and this is the primary table operators actually read.
        state: account.disabled
          ? 'disabled'
          : account.needs_relogin
            ? 'needs-relogin'
            : !account.has_state
              ? 'unseen'
              : account.cooldown_secs_remaining
                ? 'cooling'
                : account.near_quota
                  ? 'near-quota'
                  : account.cooldown_fable_secs_remaining
                    ? 'cooling-fable'
                    : 'available',
        utilization_5h: account.utilization_5h,
        reset_5h: account.reset_5h,
        utilization_7d: account.utilization_7d,
        reset_7d: account.reset_7d,
        utilization_7d_oi: account.utilization_7d_oi,
        reset_7d_oi: account.reset_7d_oi,
      };
      groupFor(provider).push(row);
      // `uuidByName` is sourced from the Claude account store only (see
      // `/admin/api/accounts`), so only `claude_oauth` accounts may be matched
      // against it. Gate on the account's actual auth kind (`table.auth`), not
      // the provider's display name or group key: a provider table can be named
      // anything, so a `chatgpt_oauth` provider named "claude" would otherwise
      // still get Claude uuids applied, and a `claude_oauth` provider under a
      // custom name would otherwise never get them.
      const uuid = table.auth === 'claude_oauth' ? uuidByName[account.name] : undefined;
      if (uuid) byUuid.set(uuid, row);
    }
  }

  for (const observation of observed) {
    // A missing uuid means "identity unknown" and must never match.
    const match = observation.uuid ? byUuid.get(observation.uuid) : undefined;
    if (match) {
      match.observed = observation;
      // Prefer the observed detail when present; otherwise keep the
      // plan-derived detail from the pool payload. `observed` comes from
      // `~/.claude/.credentials.json` while the pool's detail comes from the
      // account's store file or the live profile API — different sources, and a
      // null observed detail must not blank out a real plan-derived one.
      if (observation.detail) match.detail = observation.detail;
      // Prefer the client's windows: the pool only learns a window from a
      // response header it has actually received, so it reports null for
      // windows the client can already see.
      for (const key of WINDOW_KEYS) {
        const value = observation[key];
        if (value !== null && value !== undefined) match[key] = value;
      }
      continue;
    }
    groupFor(observation.provider).push({
      provider: observation.provider,
      label: observation.identity || observation.provider,
      detail: observation.detail,
      managed: null,
      observed: observation,
      state: observation.state,
      utilization_5h: observation.utilization_5h,
      reset_5h: observation.reset_5h,
      utilization_7d: observation.utilization_7d,
      reset_7d: observation.reset_7d,
      utilization_7d_oi: observation.utilization_7d_oi,
      reset_7d_oi: observation.reset_7d_oi,
    });
  }

  return groups;
}

/**
 * The one state the label, the `data-state` used for styling, and the
 * remediation note all read.
 *
 * Coalescing lets an observed row override a managed row's displayed status
 * (e.g. the client sees "expired" quota the pool has not detected yet). Reading
 * three separate derivations would let them disagree — a "Needs login" label
 * next to a green "available" dot with no login hint.
 */
export function effectiveState(row: AccountRow): string {
  // Managed pool operational states are actionable gateway-side facts (the pool
  // disabled the account, is cooling it down — account-wide or for Fable only —
  // or sees it near quota) and must win over a stale local observation error: an
  // account the pool has already benched should not read "Needs login", with no
  // cooldown remediation, just because the last local check happened to see an
  // expired token.
  if (
    row.state === 'disabled' ||
    row.state === 'needs-relogin' ||
    row.state === 'cooling' ||
    row.state === 'near-quota' ||
    row.state === 'cooling-fable'
  ) {
    return row.state;
  }
  const observation = row.observed;
  if (observation) {
    if (observation.state === 'expired') return 'expired';
    if (observation.state === 'unavailable') return 'unavailable';
    if (observation.state === 'waiting-for-traffic') return 'waiting-for-traffic';
    if (observation.signal === 'integration-pending') return 'connected';
  }
  return row.state;
}

export function rowStatusText(state: string): string {
  switch (state) {
    case 'needs-relogin':
      return 'Needs re-login';
    case 'expired':
      return 'Needs login';
    case 'unavailable':
      return 'Usage unavailable';
    case 'waiting-for-traffic':
      return 'Waiting for traffic';
    case 'connected':
      return 'Connected';
    case 'disabled':
      return 'Disabled';
    case 'cooling':
      return 'Cooling';
    case 'cooling-fable':
      return 'Cooling (Fable)';
    case 'near-quota':
      return 'Near quota';
    case 'unseen':
      return 'No traffic yet';
    default:
      return 'Live';
  }
}
