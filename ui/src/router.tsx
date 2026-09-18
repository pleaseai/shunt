import { Toggle } from '@base-ui/react/toggle';
import { ToggleGroup } from '@base-ui/react/toggle-group';
import {
  Link,
  Outlet,
  createBrowserHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from '@tanstack/react-router';
import { useLayoutEffect, useState, type ReactElement } from 'react';

import { API } from './api';
import { Dashboard } from './Dashboard';

type Theme = 'light' | 'dark' | 'system';
const THEME_KEY = 'shunt-admin-theme';

function readTheme(): Theme {
  try {
    const stored = localStorage.getItem(THEME_KEY);
    if (stored === 'light' || stored === 'dark' || stored === 'system') return stored;
  } catch {
    // Storage is an enhancement; blocked storage must not block the dashboard.
  }
  return 'system';
}

function applyTheme(theme: Theme): void {
  if (theme === 'system') delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
}

// Applied at module scope, not from `ThemeToggle`'s effect: the toggle does not
// mount until the asynchronous session bootstrap resolves, so a returning
// operator whose stored choice differs from their OS setting would see the
// loading state -- and sometimes the shell's first frame -- on the wrong
// palette. Running here keeps that inside the bundle, so the page needs no
// inline script and the current CSP is unchanged.
applyTheme(readTheme());

export function ThemeToggle(): ReactElement {
  const [theme, setTheme] = useState<Theme>(readTheme);

  // Layout, not passive: a passive effect runs after the browser paints, so the
  // toggle's own pressed state would commit one frame before the palette did and
  // every switch would flash the old colours. Applying it here keeps the DOM
  // mutation in the same commit as the render that caused it.
  useLayoutEffect(() => applyTheme(theme), [theme]);

  function select(values: Theme[]): void {
    const next = values[0];
    if (!next) return;
    setTheme(next);
    try {
      localStorage.setItem(THEME_KEY, next);
    } catch {
      // Keep the in-memory choice when storage is unavailable.
    }
  }

  return (
    <ToggleGroup
      aria-label="Color theme"
      className="flex rounded-lg border border-border bg-track p-0.5"
      value={[theme]}
      onValueChange={select}
    >
      {(['light', 'dark', 'system'] as const).map((value) => (
        <Toggle
          key={value}
          value={value}
          aria-label={`${value[0].toUpperCase()}${value.slice(1)} theme`}
          className="theme-option min-h-0 rounded-md border-0 bg-transparent px-2 py-1 text-[0.72rem] capitalize text-text-secondary hover:text-text data-[pressed]:bg-accent data-[pressed]:text-[#101521]"
        >
          {value}
        </Toggle>
      ))}
    </ToggleGroup>
  );
}

function SignOut(): ReactElement {
  async function signOut(): Promise<void> {
    try {
      await fetch(`${API}/logout`, { method: 'POST' });
    } catch {
      // The cookie may or may not be cleared; the login page settles it.
    }
    window.location.assign('/admin/login');
  }
  return (
    <button className="secondary" type="button" onClick={() => void signOut()}>
      Sign out
    </button>
  );
}

const nav = [
  { to: '/', label: 'Overview', available: true },
  { to: '/quota', label: 'Quota', available: false },
  { to: '/routes', label: 'Routes', available: false },
] as const;

function AppShell(): ReactElement {
  return (
    <div className="min-h-screen md:grid md:grid-cols-[13rem_minmax(0,1fr)]">
      <aside className="border-b border-border bg-card px-4 py-4 backdrop-blur-[10px] md:min-h-screen md:border-r md:border-b-0 md:px-5 md:py-7">
        <div className="text-[1.05rem] font-bold tracking-[-0.04em]">shunt</div>
        <nav aria-label="Admin" className="mt-4 flex gap-2 md:flex-col">
          {nav.map((item) => (
            <Link
              key={item.to}
              to={item.to}
              activeOptions={{ exact: true }}
              className="nav-link flex items-center justify-between gap-2 rounded-lg px-3 py-2 text-text-secondary no-underline hover:bg-track hover:text-text [&.active]:bg-track [&.active]:text-text"
            >
              {item.label}
              {!item.available ? (
                <span className="rounded border border-border px-1.5 py-0.5 text-[0.62rem] uppercase tracking-wide">
                  Soon
                </span>
              ) : null}
            </Link>
          ))}
        </nav>
      </aside>
      <div className="min-w-0">
        <header className="flex flex-wrap items-center justify-between gap-3 border-b border-border bg-card px-4 py-3 backdrop-blur-[10px] sm:px-6">
          <h1 className="text-[1.35rem] font-bold tracking-[-0.04em]">shunt admin</h1>
          <div className="flex flex-wrap items-center gap-2">
            <ThemeToggle />
            <SignOut />
          </div>
        </header>
        <main className="mx-auto max-w-[68rem] px-3.5 pt-5 pb-16 sm:px-5 sm:pt-8 sm:pb-20">
          <Outlet />
        </main>
      </div>
    </div>
  );
}

function EmptyState({ title }: { title: string }): ReactElement {
  return (
    <section className="card text-center">
      <h2 className="mt-0 text-base">{title}</h2>
      <p className="muted mb-0">Coming soon</p>
    </section>
  );
}

const rootRoute = createRootRoute({ component: AppShell });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: Dashboard });
const quotaRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/quota',
  component: () => <EmptyState title="Quota" />,
});
const routesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/routes',
  component: () => <EmptyState title="Routes" />,
});

export function createAdminRouter() {
  return createRouter({
    routeTree: rootRoute.addChildren([indexRoute, quotaRoute, routesRoute]),
    history: createBrowserHistory(),
    basepath: '/admin',
  });
}

declare module '@tanstack/react-router' {
  interface Register {
    router: ReturnType<typeof createAdminRouter>;
  }
}
