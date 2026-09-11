import { render, screen, waitFor, within } from '@testing-library/react';
import { vi } from 'vitest';

import { App } from '../App';
import type {
  ClaudeStoreAccount,
  CodexStoreAccount,
  ObservedAccount,
  PoolProvider,
  StatusSource,
} from '../types';

/** A minimal stand-in for `Response` — only what `api.ts` actually reads. */
export function reply(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response;
}

/**
 * A stand-in for an answer this surface cannot read — a proxy's error page, a
 * truncated body. `json()` rejects the way `Response.json()` does on one.
 */
export function unreadable(status = 502): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => {
      throw new SyntaxError('Unexpected token < in JSON at position 0');
    },
  } as unknown as Response;
}

export type Route = (init: RequestInit | undefined) => Response | Promise<Response>;
/** Keyed `"<METHOD> <path>"`, e.g. `"GET /admin/api/pool"`. */
export type Routes = Record<string, Route>;

export interface RecordedCall {
  method: string;
  path: string;
  headers: Record<string, string>;
  body: string | null;
}

export interface Fixtures {
  session?: { csrf: string; expiry_buffer_ms: number };
  observed?: ObservedAccount[];
  pool?: PoolProvider[];
  accounts?: ClaudeStoreAccount[];
  codexAccounts?: CodexStoreAccount[];
  status?: StatusSource[];
}

export const DEFAULT_EXPIRY_BUFFER_MS = 300_000;

function defaultRoutes(fixtures: Fixtures): Routes {
  return {
    'GET /admin/api/session': () =>
      reply(fixtures.session ?? { csrf: 'test-csrf', expiry_buffer_ms: DEFAULT_EXPIRY_BUFFER_MS }),
    'GET /admin/api/observed': () => reply({ accounts: fixtures.observed ?? [] }),
    'GET /admin/api/pool': () => reply({ providers: fixtures.pool ?? [] }),
    'GET /admin/api/accounts': () => reply({ accounts: fixtures.accounts ?? [] }),
    'GET /admin/api/accounts/codex': () => reply({ accounts: fixtures.codexAccounts ?? [] }),
    'GET /admin/api/status': () => reply({ sources: fixtures.status ?? [] }),
  };
}

export interface Api {
  calls: RecordedCall[];
  /** Every recorded call to one endpoint, newest last. */
  callsTo: (method: string, path: string) => RecordedCall[];
}

export function mockApi(routes: Routes): Api {
  const calls: RecordedCall[] = [];
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = String(input).replace(/^https?:\/\/[^/]+/, '');
    const method = (init?.method ?? 'GET').toUpperCase();
    calls.push({
      method,
      path,
      headers: (init?.headers as Record<string, string>) ?? {},
      body: typeof init?.body === 'string' ? init.body : null,
    });
    const route = routes[`${method} ${path}`];
    if (!route) throw new Error(`unrouted ${method} ${path}`);
    return route(init);
  });
  vi.stubGlobal('fetch', fetchMock);
  return {
    calls,
    callsTo: (method, path) =>
      calls.filter((call) => call.method === method.toUpperCase() && call.path === path),
  };
}

/**
 * Render the whole bundle against a stubbed API and wait for the primary table.
 *
 * `App`, not `Dashboard`: the bootstrap fetch is part of every property here —
 * the refresh buffer and the CSRF token both reach the page through it.
 */
export async function renderDashboard(fixtures: Fixtures = {}, extra: Routes = {}): Promise<Api> {
  const api = mockApi({ ...defaultRoutes(fixtures), ...extra });
  render(<App />);
  await screen.findByRole('heading', { name: 'Accounts and usage' });
  // Every table settles before a test asserts: leaving one on "Loading…" is how
  // an assertion about a missing row passes for the wrong reason.
  await waitFor(() => expect(screen.queryAllByText('Loading…')).toHaveLength(0));
  // `Upstream status` is not covered by that wait, and cannot be: it renders
  // `null` — not a "Loading…" row — until `GET /admin/api/status` resolves, so
  // the settle loop above is already satisfied while the section is still
  // pending (`UpstreamStatus`, `useDashboard`'s one-shot read). Every assertion
  // about it would then race the fetch: absence-assertions would pass for the
  // wrong reason, and presence-assertions would flake. Wait for the section
  // itself when the fixture configures sources — and only then, since with none
  // configured `null` is the settled state and there is nothing to wait for.
  if (fixtures.status?.length) {
    await screen.findByRole('heading', { name: 'Upstream status' });
  }
  return api;
}

/**
 * Queries scoped to one table body. Account names and row actions repeat across
 * the four tables, so an unscoped query matches whichever renders first.
 */
export function tbody(id: string): ReturnType<typeof within> {
  const element = document.getElementById(id);
  if (!element) throw new Error(`no tbody #${id} is rendered`);
  return within(element);
}

/** The `<th>` texts of the table a tbody belongs to, in document order. */
export function headersOf(id: string): string[] {
  const element = document.getElementById(id);
  const table = element?.closest('table');
  if (!table) throw new Error(`no table around #${id}`);
  return [...table.querySelectorAll('thead th')].map((th) => th.textContent ?? '');
}

/** The `<tr>` a cell belongs to, for asserting on a row as a unit. */
export function rowOf(cell: HTMLElement): HTMLElement {
  const row = cell.closest('tr');
  if (!row) throw new Error('cell is not inside a row');
  return row as HTMLElement;
}

/** A deferred whose promise a route can return, so a request can be held open. */
export function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}
