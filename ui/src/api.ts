/**
 * The one place this bundle names an endpoint. Every path here is under
 * `/admin/api/*`, the JSON namespace `src/admin/mod.rs` authenticates on every
 * request — the shell and the bundle files this code ships in are the only part
 * of the admin surface served without a credential.
 */
export const API = '/admin/api';

/** A read that either produced a payload or a message worth showing an operator. */
export type Fetched<T> =
  | { ok: true; data: T }
  | { ok: false; message: string; status?: number };

function errorMessage(payload: unknown): string | null {
  if (payload && typeof payload === 'object' && 'error' in payload) {
    const error = (payload as { error?: { message?: unknown } }).error;
    if (error && typeof error.message === 'string') return error.message;
  }
  return null;
}

/**
 * A GET whose failure modes an operator can act on: a transport failure and an
 * error status are both reported, and the gateway's own error message is
 * preferred over the generic one when the response carries it.
 */
export async function readJson<T>(path: string, fallback: string): Promise<Fetched<T>> {
  let response: Response;
  let payload: unknown;
  try {
    response = await fetch(path);
    payload = await response.json();
  } catch {
    return { ok: false, message: fallback };
  }
  if (!response.ok) {
    return { ok: false, message: errorMessage(payload) ?? fallback, status: response.status };
  }
  return { ok: true, data: payload as T };
}

/**
 * Headers for a mutating request. The CSRF token comes from
 * `GET /admin/api/session` rather than being interpolated into this bundle: the
 * shell is one static embedded file, identical for every visitor, so it cannot
 * carry a per-session value. A header-credential caller is CSRF-exempt and the
 * bootstrap hands it an empty string, which the guard never inspects.
 */
export function mutationHeaders(csrf: string): Record<string, string> {
  return { 'content-type': 'application/json', 'x-csrf-token': csrf };
}

/** A mutation's outcome, with the server's own message when it sent one. */
export interface MutationResult {
  ok: boolean;
  message: string | null;
  payload: Record<string, unknown>;
  /**
   * Whether the body parsed as a JSON object. Every `/admin/api/*` mutation
   * answers with one — no admin handler returns a body-less 204 — so `false`
   * means the answer did not come from the gateway (a proxy's error page, a
   * truncated body) and so reports nothing about what the request actually did.
   *
   * Only a caller whose request is unsafe to retry needs this. The rest may
   * ignore it and treat an uninterpretable answer as the failure `ok` already
   * says it is, which is what `src/admin/script.rs` does: its remove and
   * refresh handlers read the body through `.catch(() => ({}))`, while the
   * completion handler's `await res.json()` is deliberately bare so an
   * unreadable answer escalates to its unknown-outcome path.
   */
  answered: boolean;
}

export async function mutate(
  path: string,
  csrf: string,
  init: { method: string; body?: string; signal?: AbortSignal },
): Promise<MutationResult> {
  const response = await fetch(path, {
    method: init.method,
    headers: mutationHeaders(csrf),
    body: init.body,
    signal: init.signal,
  });
  let payload: Record<string, unknown> = {};
  let answered = false;
  try {
    const parsed: unknown = await response.json();
    // A JSON `null`, array, or bare primitive parses without throwing but is
    // not an answer this surface can read a field off, so it is not one.
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      payload = parsed as Record<string, unknown>;
      answered = true;
    }
  } catch {
    // A body-less or unparseable answer is still a verdict; `response.ok`
    // carries it, and `answered` tells the callers that need more than that.
  }
  return { ok: response.ok, message: errorMessage(payload), payload, answered };
}
