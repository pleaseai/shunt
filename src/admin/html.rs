//! The server-rendered admin login page (M9). No framework, no external
//! requests, no script: one HTML string with an inline stylesheet.
//!
//! This is all that remains server-rendered. The dashboard it used to sit
//! beside moved to the embedded SPA bundle (`super::ui`, `ui/`); the login flow
//! stayed here, for a reason that is not about authentication — the shell is
//! served unauthenticated too (`super::ui`). It is about availability: the
//! bundle exists only in a `--features ui` build, and an admin surface whose
//! *sign-in* page vanished with that feature would be unusable rather than
//! merely dashboard-less. `docs/admin-ui-delivery.md` Resolution 6 leaves
//! `/admin/login` and `/admin/oidc/callback` on their original paths for the
//! separate reason that they are pages, not JSON with somewhere to move to.

/// Escape the few characters that matter when interpolating a value into HTML
/// text or a double-quoted attribute. The login page has two such values — the
/// error message and the SSO button's label — and nothing else on this surface
/// is server-interpolated any more.
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The login page's stylesheet, inlined because the page is a single HTML
/// string with no asset route of its own.
///
/// `ui/src/index.css` is a copy of this, for the embedded SPA bundle, which
/// serves it as an external stylesheet instead — that is what lets the shell's
/// Content-Security-Policy drop `'unsafe-inline'` for styles (`ui.rs`). Keep the
/// two in step: the sign-in page and the dashboard are now rendered by two
/// different mechanisms, and a change here that the bundle does not get makes
/// them look like two products.
const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #1a1f2e; --text: #e8f0ff; --text-secondary: #a8b8d0;
  --accent: #6aa7ff; --accent-light: #8ac7ff; --border: rgba(58,69,88,.9);
  --card: rgba(42,53,72,.62); --track: rgba(22,27,40,.85);
  --shadow: 0 10px 30px rgba(0,0,0,.18); --danger: #ff8b96;
}
* { box-sizing: border-box; }
body { min-height: 100vh; margin: 0; font-family: "Fragment Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 13px; line-height: 1.55; letter-spacing: -.15px; color: var(--text);
  background: radial-gradient(ellipse 140% 80% at 50% -5%, #1e3d72 0%, var(--bg) 58%) fixed; }
main { max-width: 68rem; margin: 0 auto; padding: 2rem 1.25rem 5rem; }
h1 { font-size: 1.35rem; letter-spacing: -.04em; } h2 { font-size: 1rem; margin-top: 2.4rem; }
header { display: flex; align-items: center; justify-content: space-between; }
.card { margin-top: 1rem; padding: 1rem 1.1rem; border: 1px solid var(--border); border-radius: 12px;
  background: var(--card); box-shadow: var(--shadow); backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px); }
label { display: block; font-size: .85rem; margin: .5rem 0 .2rem; }
input, textarea, button { font: inherit; }
input, textarea { width: 100%; padding: .55rem .65rem; border: 1px solid var(--border); border-radius: 8px;
  background: var(--track); color: inherit; }
@media (max-width: 40rem) { input, textarea { font-size: 1rem; } }
fieldset { border: 0; padding: 0; margin: .7rem 0; }
legend { font-size: .85rem; margin-bottom: .25rem; }
.choice { display: flex; gap: .45rem; align-items: flex-start; margin: .25rem 0; padding: .2rem 0; }
.choice input { flex: 0 0 auto; width: auto; margin: .2rem 0 0; }
.choice span, .choice small { display: block; } .choice small { margin-top: .1rem; }
.choice input:disabled { opacity: .55; cursor: not-allowed; }
.choice input:disabled ~ span { color: var(--text-secondary); }
textarea { min-height: 4.5rem; font-family: inherit; }
button { min-height: 2.65rem; padding: .5rem .9rem; cursor: pointer; touch-action: manipulation;
  border: 1px solid var(--accent); border-radius: 8px; background: var(--accent); color: #101521; }
button:focus-visible, input:focus-visible, textarea:focus-visible, .choice:has(input:focus-visible), summary:focus-visible {
  outline: 2px solid var(--accent-light); outline-offset: 3px; }
button.secondary { background: transparent; color: inherit; border-color: var(--border); }
button.danger { min-height: 0; background: transparent; color: var(--danger); border-color: color-mix(in srgb, var(--danger) 55%, transparent); padding: .25rem .5rem; }
button.compact { min-height: 0; padding: .25rem .5rem; }
.row-actions { white-space: nowrap; } .row-actions button + button { margin-left: .4rem; }
table { width: 100%; border-collapse: collapse; font-size: .88rem; }
th, td { text-align: left; vertical-align: top; padding: .72rem .55rem; border-bottom: 1px solid rgba(128,144,168,.22); }
th { color: var(--text-secondary); font-weight: 600; } tbody tr:last-child td { border-bottom: 0; }
code, .mono { font-family: inherit; font-size: .85em; }
.msg { padding: .6rem .8rem; border-radius: 8px; margin-top: .6rem; font-size: .9rem; }
.msg.err { background: #ff5a6b22; } .msg.ok { background: #6aa7ff22; }
.muted { color: var(--text-secondary); } .row { display: flex; gap: .6rem; align-items: end; }
.provider { display: inline-flex; align-items: center; gap: .55rem; font-weight: 600; white-space: nowrap; }
.provider-logo { width: 1.15rem; height: 1.15rem; flex: 0 0 auto; color: var(--text); }
.account-detail, .status-note { display: block; margin-top: .18rem; color: var(--text-secondary); font-size: .76rem; line-height: 1.35; }
.status { white-space: nowrap; font-weight: 600; }
.status[data-state="available"]::before { content: ""; display: inline-block; width: .46rem; height: .46rem; margin-right: .42rem; border-radius: 50%; background: var(--accent); }
.status[data-state="expired"], .status[data-state="unavailable"], .status[data-state="needs-relogin"] { color: var(--danger); }
.status[data-state="minor"] { color: var(--accent-light); }
.status[data-state="major"], .status[data-state="critical"], .status[data-state="unknown"] { color: var(--danger); }
.usage-lines { min-width: 24rem; }
.usage-item + .usage-item { margin-top: .62rem; }
.usage-meta { display: flex; justify-content: space-between; gap: 1rem; margin-bottom: .26rem; font-size: .78rem; }
.usage-value { color: var(--text-secondary); white-space: nowrap; }
.usage-track { height: .42rem; overflow: hidden; border-radius: 999px; background: var(--track); }
.usage-fill { height: 100%; border-radius: inherit; background: linear-gradient(90deg, var(--accent), var(--accent-light)); }
.usage-fill[data-level="full"] { background: linear-gradient(90deg, #ff6e7d, #ff9a8f); }
.usage-empty { color: var(--text-secondary); font-size: .82rem; }
.pending-row { opacity: .68; }
.overflow { overflow-x: auto; }
details { margin-top: 2rem; } summary { cursor: pointer; color: var(--text-secondary); } summary strong { color: var(--text); }
a { color: var(--accent-light); }
@media (max-width: 48rem) {
  main { padding: 1.2rem .8rem 4rem; } header { margin-bottom: 2rem; }
  .card { padding: .5rem; } .overflow { overflow: visible; }
  #observed { display: block; } #observed tr { display: grid; grid-template-columns: minmax(0,.72fr) minmax(0,1.28fr); gap: .55rem .75rem;
    padding: .85rem .45rem; border-bottom: 1px solid rgba(128,144,168,.25); }
  #observed tr:last-child { border-bottom: 0; }
  #observed td { display: block; min-width: 0; padding: 0; border: 0; overflow-wrap: anywhere; }
  #observed td:nth-child(3), #observed td:nth-child(4) { grid-column: 1 / -1; }
  #observed td:nth-child(3) { padding-top: .2rem; }
  #observed td:nth-child(4) { padding-top: .3rem; }
  #observed-table thead { display: none; }
  .usage-lines { min-width: 0; } .account-detail { display: block; }
  .usage-meta { font-size: .76rem; } .status { white-space: normal; } .row-actions { white-space: normal; }
}
@media (prefers-color-scheme: light) {
  :root { --bg: #fff; --text: #1a1f2e; --text-secondary: #5a6a7e; --border: rgba(208,216,224,.95);
    --card: rgba(255,255,255,.78); --track: #e8ecf2; --shadow: 0 10px 28px rgba(0,0,0,.10); --danger: #b42336; }
  body { background: radial-gradient(ellipse 130% 70% at 50% -5%, #ddeafe 0%, #fff 55%) fixed; }
}
@media (forced-colors: active) { .usage-track { border: 1px solid CanvasText; } .usage-fill { background: Highlight; } }
"#;

/// The login form. `error` is shown above the form when a prior attempt failed.
/// When configured, `sso_label` adds an external identity-provider sign-in form.
pub fn login_page(error: Option<&str>, sso_label: Option<&str>) -> String {
    let error_block = match error {
        Some(message) => format!(r#"<div class="msg err">{}</div>"#, escape_html(message)),
        None => String::new(),
    };
    let sso_form = sso_label.map_or_else(String::new, |label| {
        format!(
            r#"<form method="post" action="/admin/api/oidc/start" style="margin-top:.8rem">
<button class="secondary" type="submit">{}</button>
</form>"#,
            escape_html(label)
        )
    });
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>shunt admin — sign in</title><style>{STYLE}</style></head><body><main>
<h1>shunt admin</h1>
<div class="card" style="max-width:24rem">
{error_block}
<form method="post" action="/admin/login">
<label for="token">Admin token</label>
<input id="token" name="token" type="password" autocomplete="current-password" autofocus>
<div style="margin-top:.8rem"><button type="submit">Sign in</button></div>
</form>
{sso_form}
</div>
<p class="muted" style="margin-top:1rem;font-size:.85rem">Provisions upstream Claude and Codex accounts and shows pool health. Bind behind HTTPS/a tunnel.</p>
</main></body></html>"#
    )
}
