/**
 * Placeholder shell. The existing dashboard is still server-rendered from Rust
 * string literals (`src/admin/html.rs`) and `GET /admin` still serves it; this
 * scaffold only proves the bundle builds, embeds, and is served from the
 * `/admin` mount. Porting the views is the next step of the admin UI track.
 */
export function App() {
  return (
    <main>
      <h1>shunt admin</h1>
      <p>The operator dashboard is being ported to this bundle.</p>
    </main>
  );
}
