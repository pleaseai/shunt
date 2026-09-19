import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { App } from '../App';
import { DEFAULT_EXPIRY_BUFFER_MS, mockApi, reply } from '../test/harness';

/** The session bootstrap every route needs before the shell renders at all. */
function stubSession(): void {
  mockApi({
    'GET /admin/api/session': () =>
      reply({ csrf: 'test-csrf', expiry_buffer_ms: DEFAULT_EXPIRY_BUFFER_MS, access: 'write' }),
  });
}

afterEach(() => {
  window.history.pushState({}, '', '/admin');
  delete document.documentElement.dataset.theme;
  localStorage.clear();
});

describe('admin shell routing', () => {
  /**
   * The Rust side answers the SPA shell for any unmatched path under the
   * `/admin` mount, so a typed or bookmarked deep link arrives here as a full
   * page load rather than as a client-side navigation. If the router's basepath
   * and the mount disagreed, these would render the index route instead.
   */
  it.each([
    ['/admin/quota', 'Quota'],
    ['/admin/routes', 'Routes'],
  ])('renders %s as its own placeholder route', async (path, title) => {
    stubSession();
    window.history.pushState({}, '', path);

    render(<App />);

    expect(await screen.findByRole('heading', { name: title })).toBeInTheDocument();
    expect(screen.getByText('Coming soon')).toBeInTheDocument();
    // The index route's content must not leak into a sibling route.
    expect(screen.queryByRole('heading', { name: 'Accounts and usage' })).toBeNull();
  });
});

describe('theme toggle', () => {
  /**
   * The stored preference has to reach the DOM when the bundle runs, not when
   * `ThemeToggle` mounts: the toggle appears only after the session bootstrap
   * resolves, so a returning operator whose choice differs from their OS
   * setting would otherwise watch the loading state in the wrong palette.
   *
   * Importing the module *is* the behaviour under test -- the call sits at
   * module scope and runs once per module instance -- so rendering `<App />`
   * again would prove nothing. Reset the registry and re-import with the
   * preference already seeded; deleting that line turns this red.
   */
  it('applies the stored theme when the module loads, before the toggle mounts', async () => {
    vi.resetModules();
    localStorage.setItem('shunt-admin-theme', 'dark');

    await import('../router');

    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('applies the chosen theme and remembers it', async () => {
    stubSession();
    render(<App />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole('button', { name: 'Light theme' }));

    expect(document.documentElement.dataset.theme).toBe('light');
    expect(localStorage.getItem('shunt-admin-theme')).toBe('light');

    // `system` is the absence of an override, not a third stored value applied
    // to the element -- otherwise it would pin the light/dark choice the OS is
    // supposed to make.
    await user.click(screen.getByRole('button', { name: 'System theme' }));
    expect(document.documentElement.dataset.theme).toBeUndefined();
  });

  /**
   * Storage throws outright in a private window and with site data blocked.
   * The theme is an enhancement, so that must cost the operator the memory of
   * their choice and nothing else -- not the dashboard.
   */
  it('still switches theme when storage throws', async () => {
    stubSession();
    const denied = new Error('storage is blocked');
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw denied;
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw denied;
    });

    render(<App />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole('button', { name: 'Dark theme' }));

    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(screen.getByRole('heading', { name: 'shunt admin' })).toBeInTheDocument();
  });
});
