/**
 * Display helpers. The originals lived in the server-rendered dashboard's inline
 * script; `esc` has no counterpart here because React escapes text nodes by
 * construction, which is what made the old `textContent`-only discipline
 * necessary in the first place.
 */

export function pct(value: number | null | undefined): string {
  return value === null || value === undefined ? '—' : `${Math.round(value * 100)}%`;
}

/**
 * Mirrors the Rust `title_case` helper (`src/auth/observation.rs`): capitalize
 * only the first character, matching the wording the observed-accounts view
 * already shows for a plan (e.g. "Max plan"), not per-word title casing.
 */
export function titleCase(value: string | null | undefined): string {
  return value ? value.charAt(0).toUpperCase() + value.slice(1) : '';
}

/** A coarse "time until" for a reset instant given in whole seconds. */
export function untilShort(resetSecs: number): string {
  const target = resetSecs * 1000;
  const mins = Math.ceil((target - Math.min(Date.now(), target)) / 60000);
  if (mins <= 0) return 'now';
  const days = Math.floor(mins / 1440);
  const hours = Math.floor((mins % 1440) / 60);
  const rest = mins % 60;
  if (days > 0) return hours > 0 ? `${days}d ${hours}h` : `${days}d`;
  if (hours > 0) return rest > 0 ? `${hours}h ${rest}m` : `${hours}h`;
  return `${rest}m`;
}

export function pctReset(value: number | null | undefined, resetSecs: number | null | undefined): string {
  return resetSecs ? `${pct(value)} · ${untilShort(resetSecs)}` : pct(value);
}

export function when(ms: number | null | undefined): string {
  return ms ? new Date(ms).toLocaleString() : '—';
}
