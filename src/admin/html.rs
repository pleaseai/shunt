//! Server-rendered admin pages (M9). No framework, no external requests: inline
//! CSS and a small inline script that drives the Claude and Codex add-account
//! flows and sends the CSRF token as `x-csrf-token`. All account/pool data is
//! rendered with `textContent` in the script (never `innerHTML`), so
//! upstream-derived strings cannot inject markup.

/// Escape the few characters that matter when interpolating a value into HTML
/// text or a double-quoted attribute. Used only for the login error and the CSRF
/// token; all other dynamic content is set client-side via `textContent`.
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
textarea { min-height: 4.5rem; font-family: inherit; }
button { min-height: 2.65rem; padding: .5rem .9rem; cursor: pointer; touch-action: manipulation;
  border: 1px solid var(--accent); border-radius: 8px; background: var(--accent); color: #101521; }
button:focus-visible, input:focus-visible, textarea:focus-visible, .choice:has(input:focus-visible), summary:focus-visible {
  outline: 2px solid var(--accent-light); outline-offset: 3px; }
button.secondary { background: transparent; color: inherit; border-color: var(--border); }
button.danger { min-height: 0; background: transparent; color: var(--danger); border-color: color-mix(in srgb, var(--danger) 55%, transparent); padding: .25rem .5rem; }
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
.status[data-state="expired"], .status[data-state="unavailable"] { color: var(--danger); }
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
  .usage-meta { font-size: .76rem; } .status { white-space: normal; }
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
            r#"<form method="post" action="/admin/oidc/start" style="margin-top:.8rem">
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

/// The authenticated dashboard. `csrf` is embedded for the inline script to send
/// on mutating requests.
pub fn dashboard_page(csrf: &str) -> String {
    let csrf = escape_html(csrf);
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>shunt admin</title><style>{STYLE}</style></head><body><main>
<header><h1>shunt admin</h1>
<form method="post" action="/admin/logout"><button class="secondary" type="submit">Sign out</button></form>
</header>

<h2>Accounts and usage</h2>
<p class="muted">Read-only signals from provider clients on this machine. <strong>Waiting for traffic</strong> means GPT has not returned quota headers to this shunt yet; <strong>Needs login</strong> means the provider-owned access token expired and must be renewed by that provider client.</p>
<div class="card overflow"><table id="observed-table"><thead><tr><th>Provider</th><th>Account</th><th>Status</th><th>Usage</th></tr></thead>
<tbody id="observed"><tr><td colspan="4" class="muted">Loading…</td></tr></tbody></table></div>

<details style="margin-top:2rem"><summary><strong>Manage pool accounts</strong> <span class="muted">(advanced)</span></summary>
<p class="muted">Managed accounts are separate credential copies owned and refreshed by shunt for load-balancing. You do not need them merely to view usage.</p>
<h2>Add Claude account</h2>
<div class="card">
<p id="modehelp" class="muted" style="margin-top:0">Full OAuth creates a refreshable login that shunt manages.</p>
<label for="name">Account name <span class="muted">(lowercase letters, digits, hyphens)</span></label>
<input id="name" name="name" placeholder="e.g. pool-b" autocomplete="off" spellcheck="false">
<fieldset>
<legend>Login method</legend>
<label class="choice"><input id="mode-oauth" type="radio" name="mode" value="oauth" checked>
<span>Full OAuth (refreshable)</span></label>
<label class="choice"><input id="mode-setup" type="radio" name="mode" value="setup_token">
<span>Setup token (1-year, inference-only)</span></label>
</fieldset>
<button id="start" type="button">Start account login</button>
<div id="step2" style="display:none;margin-top:1rem">
<p>1. Open this URL, sign in to the target Claude account, and approve:</p>
<p class="overflow"><a id="authlink" target="_blank" rel="noopener noreferrer"></a></p>
<label for="code">2. Paste the code shown after approval (<code>&lt;code&gt;#&lt;state&gt;</code>)</label>
<textarea id="code"></textarea>
<div style="margin-top:.6rem"><button id="complete" type="button">Complete</button></div>
</div>
<div id="addmsg" aria-live="polite"></div>
</div>

<h2>Add Codex account</h2>
<div class="card">
<p class="muted" style="margin-top:0">ChatGPT OAuth creates a refreshable login that shunt manages.</p>
<label for="codex-name">Account name <span class="muted">(lowercase letters, digits, hyphens)</span></label>
<input id="codex-name" name="codex-name" placeholder="e.g. codex-backup" autocomplete="off" spellcheck="false">
<button id="start-codex" type="button" style="margin-top:.7rem">Start Codex login</button>
<div id="codex-step2" style="display:none;margin-top:1rem">
<p>1. Open this URL, sign in to the target ChatGPT account, and approve:</p>
<p class="overflow"><a id="codex-authlink" target="_blank" rel="noopener noreferrer"></a></p>
<p class="muted">The localhost callback page will fail to load. This is expected; copy the full URL from the browser address bar.</p>
<label for="codex-code">2. Paste the full redirected URL from the browser address bar</label>
<textarea id="codex-code" name="codex-code" spellcheck="false" placeholder="http://localhost:1455/auth/callback?code=…&state=…"></textarea>
<div style="margin-top:.6rem"><button id="complete-codex" type="button">Complete Codex login</button></div>
</div>
<div id="codex-addmsg" aria-live="polite"></div>
</div>

<h2>Claude accounts</h2>
<div class="card overflow"><table><thead><tr><th>Name</th><th>Kind</th><th>Expires</th><th>UUID</th><th></th></tr></thead>
<tbody id="accounts"><tr><td colspan="5" class="muted">Loading…</td></tr></tbody></table></div>

<h2>Codex accounts</h2>
<div class="card overflow"><table><thead><tr><th>Name</th><th>Expires</th><th>Account ID</th><th></th></tr></thead>
<tbody id="codex-accounts"><tr><td colspan="4" class="muted">Loading…</td></tr></tbody></table></div>

<h2>Managed pool health</h2>
<div class="card overflow"><table><thead><tr><th>Provider</th><th>Account</th><th>State</th><th>5h</th><th>7d</th><th>7d_oi</th><th>Status</th><th>Cooldown</th></tr></thead>
<tbody id="pool"><tr><td colspan="8" class="muted">Loading…</td></tr></tbody></table></div>
</details>

<script>
const CSRF = "{csrf}";
const H = {{ "content-type": "application/json", "x-csrf-token": CSRF }};
const $ = (id) => document.getElementById(id);
function esc(v) {{ return v === null || v === undefined ? "" : String(v); }}
function pct(v) {{ return v === null || v === undefined ? "—" : Math.round(v * 100) + "%"; }}
function untilShort(resetSecs) {{
  const mins = Math.ceil((resetSecs * 1000 - Math.min(Date.now(), resetSecs * 1000)) / 60000);
  if (mins <= 0) return "now";
  const d = Math.floor(mins / 1440), h = Math.floor((mins % 1440) / 60), m = mins % 60;
  return d > 0 ? (h > 0 ? d + "d " + h + "h" : d + "d") : h > 0 ? (m > 0 ? h + "h " + m + "m" : h + "h") : m + "m";
}}
function pctReset(v, resetSecs) {{
  return resetSecs ? pct(v) + " · " + untilShort(resetSecs) : pct(v);
}}
function when(ms) {{ return ms ? new Date(ms).toLocaleString() : "—"; }}
function cell(row, text, mono) {{ const td = document.createElement("td"); td.textContent = esc(text);
  if (mono) td.className = "mono"; row.appendChild(td); return td; }}

function providerLabel(provider) {{ return ({{ claude: "Claude", codex: "GPT", grok: "Grok", kimi: "Kimi", gemini: "Gemini", cursor: "Cursor" }})[provider] || provider; }}
const PROVIDER_ICONS = {{
  claude: ["0 0 24 24", "m4.714 15.956 4.718-2.648.079-.23-.079-.128H9.2l-6.866-.34-1.142-.243-.534-.704.055-.352.48-.322 8.116.558.158-.158-7.068-4.91-.722-.492-.364-.461-.158-1.008.656-.722.88.06 8.5 6.338.158-.073-3.825-7.073-.17-.62c-.061-.255-.104-.467-.104-.728L6.287.134 6.7 0l.996.134.419.364 3.868 8.402.158.255h.158l.364-7.255.377-.91.747-.492.583.28.48.685-1.275 7.012h.212l4.44-5.135.85-.905h1.032l.759 1.13-.34 1.165-4.845 6.547.073.11 6.266-1.202.832.389.091.394-.328.807-7.534 1.76.049.061 5.849.407.789.522.474.638-.079.486-1.214.619-5.318-1.226h-.182v.109l6.854 6.22.127.577-.321.455-.34-.048-5.747-4.768h-.128v.17l2.908 4.171.121 1.081-.17.352-.607.213-.668-.122-3.369-4.808-1.141-1.943-.14.079-.674 7.255-.315.37-.729.28-.607-.462-.322-.747 1.603-7.023-.012-.043-.14.019-5.336 7.322-.413.164-.716-.37.067-.662.4-.589 4.754-6.004-.006-.158h-.055l-6.338 4.117-1.13.145-.485-.455.06-.747.231-.243Z"],
  codex: ["0 0 24 24", "M22.282 9.821a5.985 5.985 0 0 0-.516-4.911 6.046 6.046 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.182a5.985 5.985 0 0 0-3.998 2.9 6.046 6.046 0 0 0 .743 7.096 5.98 5.98 0 0 0 .511 4.911 6.051 6.051 0 0 0 6.515 2.9A5.985 5.985 0 0 0 13.26 24a6.056 6.056 0 0 0 5.772-4.206 5.989 5.989 0 0 0 3.997-2.9 6.056 6.056 0 0 0-.747-7.073Zm-9.022 12.608a4.476 4.476 0 0 1-2.877-1.041l4.92-2.839a.795.795 0 0 0 .393-.68v-6.737l2.02 1.168.038.052v5.583a4.504 4.504 0 0 1-4.494 4.494Zm-9.661-4.125a4.471 4.471 0 0 1-.535-3.014l4.925 2.843a.771.771 0 0 0 .781 0l5.843-3.369v2.333l-.033.061-4.84 2.792a4.499 4.499 0 0 1-6.141-1.646ZM2.341 7.896a4.485 4.485 0 0 1 2.365-1.973V11.6a.766.766 0 0 0 .388.677l5.814 3.354-2.02 1.169h-.071l-4.83-2.787a4.504 4.504 0 0 1-1.646-6.141Zm16.596 3.856-5.833-3.388L15.119 7.2h.071l4.83 2.791a4.494 4.494 0 0 1-.676 8.104v-5.677a.79.79 0 0 0-.407-.667Zm2.011-3.024-4.916-2.867a.776.776 0 0 0-.785 0L9.409 9.23V6.897l.028-.061 4.83-2.787a4.499 4.499 0 0 1 6.681 4.66ZM8.307 12.863l-2.02-1.164-.038-.057V6.074a4.499 4.499 0 0 1 7.376-3.453L8.704 5.459a.795.795 0 0 0-.393.682Zm1.097-2.365 2.602-1.5 2.607 1.5v2.999l-2.597 1.5-2.607-1.5Z"],
  gemini: ["0 0 24 24", "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81"],
  kimi: ["0 0 24 24", "m1.053 16.91 9.538 2.55q-.03 1.02.06 2.031l5.956 1.592A12 11.99 0 0 1 1.053 16.91M.033 11.12l11.352 3.036q-.3.99-.469 2.01l10.817 2.89a12 11.99 0 0 1-1.845 2.004L.658 15.918a12 11.99 0 0 1-.625-4.796m1.593-5.146L13.573 9.17q-.57.9-1.01 1.874l11.297 3.02q-.24 1.2-.67 2.362L.125 10.26a12 11.99 0 0 1 1.5-4.285ZM6.067 1.58l11.285 3.016q-.9.78-1.688 1.719l7.824 2.091q.42 1.29.513 2.664L2.107 5.218a12 11.99 0 0 1 3.96-3.638M21.68 4.866 7.222 1.003A12 11.99 0 0 1 21.68 4.866"],
  cursor: ["0 0 24 24", "M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23"],
  grok: ["0 0 24 24", "M7.75 14.66 14 9.85c.3-.24.75-.15.89.22a5.4 5.4 0 0 1-6.71 7.01l-2.12 1.02a6.9 6.9 0 0 0 11.25-7.63c-.77-3.45.19-4.83 2.15-7.65l.54-.78-2.58 2.69L7.75 14.66Zm-1.29 1.17a5.4 5.4 0 0 1 .06-7.48 5.45 5.45 0 0 1 5.6-1.16l2.11-1.02a6.9 6.9 0 0 0-9.05 11.68c.8 2.03-.51 3.46-1.83 4.91-.47.51-.94 1.03-1.31 1.57l4.42-4.12Z"]
}};
function providerCell(row, provider) {{
  const td = document.createElement("td"), wrap = document.createElement("span"); wrap.className = "provider";
  const icon = PROVIDER_ICONS[provider];
  if (icon) {{ const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg"); svg.classList.add("provider-logo");
    svg.setAttribute("viewBox", icon[0]); svg.setAttribute("aria-hidden", "true");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path"); path.setAttribute("d", icon[1]); path.setAttribute("fill", "currentColor"); svg.appendChild(path); wrap.appendChild(svg); }}
  const label = document.createElement("span"); label.textContent = providerLabel(provider); wrap.appendChild(label); td.appendChild(wrap); row.appendChild(td);
}}
function usageBar(parent, label, remaining, resetTime) {{
  const used = Math.max(0, Math.min(100, Math.round((1 - remaining) * 1000) / 10));
  const item = document.createElement("div"); item.className = "usage-item";
  const meta = document.createElement("div"); meta.className = "usage-meta";
  const name = document.createElement("span"); name.textContent = label;
  const value = document.createElement("span"); value.className = "usage-value";
  value.textContent = used + "% used" + (resetTime ? " · " + untilShort(Date.parse(resetTime) / 1000) : "");
  meta.append(name, value); item.appendChild(meta);
  const track = document.createElement("div"); track.className = "usage-track"; track.setAttribute("role", "progressbar");
  track.setAttribute("aria-label", label + " usage"); track.setAttribute("aria-valuemin", "0"); track.setAttribute("aria-valuemax", "100"); track.setAttribute("aria-valuenow", used);
  const fill = document.createElement("div"); fill.className = "usage-fill"; fill.style.width = used + "%"; if (used >= 100) fill.dataset.level = "full";
  track.appendChild(fill); item.appendChild(track); parent.appendChild(item);
}}

async function loadObserved() {{
  const body = $("observed"); body.textContent = "";
  let data, res;
  try {{ res = await fetch("/admin/observed"); data = await res.json(); }}
  catch (e) {{ const r = body.insertRow(); const c = cell(r, "Failed to observe local accounts"); c.colSpan = 4; return; }}
  if (!res.ok) {{ const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to observe local accounts"); c.colSpan = 4; return; }}
  const list = (data && data.accounts) || [];
  if (!list.length) {{ const r = body.insertRow(); const c = cell(r, "No supported local provider login found. Sign in with a provider CLI."); c.colSpan = 4; c.className = "muted"; return; }}
  for (const a of list) {{
    const r = body.insertRow();
    providerCell(r, a.provider);
    const identity = cell(r, a.identity || a.provider);
    if (a.detail) {{ const detail = document.createElement("small"); detail.className = "account-detail"; detail.textContent = a.detail; identity.appendChild(detail); }}
    identity.title = a.source + " · read-only";
    const pending = a.signal === "integration-pending";
    if (pending) r.className = "pending-row";
    const statusText = a.state === "expired" ? "Needs login"
      : a.state === "unavailable" ? "Usage unavailable"
      : a.state === "waiting-for-traffic" ? "Waiting for traffic"
      : pending ? "Connected"
      : "Live";
    const status = cell(r, statusText); status.className = "status"; status.dataset.state = a.state;
    const statusNote = document.createElement("small"); statusNote.className = "status-note";
    if (a.state === "waiting-for-traffic") statusNote.textContent = "Quota arrives in GPT response headers";
    else if (a.state === "expired") statusNote.textContent = "The provider client owns refresh";
    else if (a.state === "unavailable") statusNote.textContent = "Current login could not read quota";
    if (statusNote.textContent) status.appendChild(statusNote);
    if (a.message) status.title = a.message;
    const usage = document.createElement("td"); usage.className = "usage-lines"; r.appendChild(usage);
    const buckets = (a.quota_buckets || []).filter(b => b.remaining !== null && b.remaining !== undefined);
    if (buckets.length) {{
      for (const b of buckets) usageBar(usage, b.label, b.remaining, b.reset_time);
      usage.title = buckets.map(b => b.label + ": " + Math.round((1 - b.remaining) * 1000) / 10 + "% used" + (b.reset_time ? ", resets " + new Date(b.reset_time).toLocaleString() : "")).join("\n");
    }} else {{
      const windows = [
        ["5h", a.utilization_5h, a.reset_5h],
        ["Week", a.utilization_7d, a.reset_7d],
        ["Fable", a.utilization_7d_oi, a.reset_7d_oi]
      ].filter(window => window[1] !== null && window[1] !== undefined);
      for (const window of windows) usageBar(usage, window[0], 1 - window[1], window[2] ? new Date(window[2] * 1000).toISOString() : null);
      if (!windows.length) {{ const empty = document.createElement("span"); empty.className = "usage-empty";
        empty.textContent = a.state === "expired" ? "Sign in again with the provider client"
          : a.state === "waiting-for-traffic" ? "Send one GPT request through this shunt"
          : pending ? "Usage integration in progress"
          : "No usage reported yet"; usage.appendChild(empty); }}
    }}
  }}
}}

async function loadAccounts() {{
  const body = $("accounts"); body.textContent = "";
  let data, res;
  try {{ res = await fetch("/admin/accounts"); data = await res.json(); }}
  catch (e) {{ const r = body.insertRow(); const c = cell(r, "Failed to load accounts"); c.colSpan = 5; return; }}
  if (!res.ok) {{ const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to load accounts"); c.colSpan = 5; return; }}
  const list = (data && data.accounts) || [];
  if (!list.length) {{ const r = body.insertRow(); const c = cell(r, "No store accounts yet"); c.colSpan = 5; c.className = "muted"; return; }}
  for (const a of list) {{
    const r = body.insertRow();
    cell(r, a.name); cell(r, a.kind); cell(r, when(a.expires_at)); cell(r, a.uuid || "—", true);
    const td = document.createElement("td");
    const btn = document.createElement("button"); btn.className = "danger"; btn.textContent = "Remove";
    btn.onclick = () => removeAccount(a.name); td.appendChild(btn); r.appendChild(td);
  }}
}}

async function loadCodexAccounts() {{
  const body = $("codex-accounts"); body.textContent = "";
  let data, res;
  try {{ res = await fetch("/admin/accounts/codex"); data = await res.json(); }}
  catch (e) {{ const r = body.insertRow(); const c = cell(r, "Failed to load Codex accounts"); c.colSpan = 4; return; }}
  if (!res.ok) {{ const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to load Codex accounts"); c.colSpan = 4; return; }}
  const list = (data && data.accounts) || [];
  if (!list.length) {{ const r = body.insertRow(); const c = cell(r, "No Codex store accounts yet"); c.colSpan = 4; c.className = "muted"; return; }}
  for (const a of list) {{
    const r = body.insertRow();
    cell(r, a.name); cell(r, when(a.expires_at)); cell(r, a.account_id || "—", true);
    const td = document.createElement("td");
    const btn = document.createElement("button"); btn.className = "danger"; btn.textContent = "Remove";
    btn.onclick = () => removeCodexAccount(a.name); td.appendChild(btn); r.appendChild(td);
  }}
}}

async function loadPool() {{
  const body = $("pool"); body.textContent = "";
  let data, res;
  try {{ res = await fetch("/admin/pool"); data = await res.json(); }}
  catch (e) {{ const r = body.insertRow(); const c = cell(r, "Failed to load pool"); c.colSpan = 8; return; }}
  if (!res.ok) {{ const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to load pool"); c.colSpan = 8; return; }}
  const providers = (data && data.providers) || [];
  let rows = 0;
  for (const p of providers) for (const a of (p.accounts || [])) {{
    rows++; const r = body.insertRow();
    cell(r, p.provider); cell(r, a.name);
    cell(r, a.disabled ? "disabled" : !a.has_state ? "unseen" : a.near_quota ? "near quota" : a.cooldown_secs_remaining ? "cooling" : "available");
    const c5 = cell(r, pctReset(a.utilization_5h, a.reset_5h));
    if (a.reset_5h) c5.title = "resets " + new Date(a.reset_5h * 1000).toLocaleString();
    const c7 = cell(r, pctReset(a.utilization_7d, a.reset_7d));
    if (a.reset_7d) c7.title = "resets " + new Date(a.reset_7d * 1000).toLocaleString();
    const c7oi = cell(r, pctReset(a.utilization_7d_oi, a.reset_7d_oi));
    if (a.reset_7d_oi) c7oi.title = "resets " + new Date(a.reset_7d_oi * 1000).toLocaleString();
    cell(r, a.status || "—");
    cell(r, a.cooldown_secs_remaining ? a.cooldown_secs_remaining + "s" : "—");
  }}
  if (!rows) {{ const r = body.insertRow(); const c = cell(r, "No pooled accounts configured"); c.colSpan = 8; c.className = "muted"; }}
}}

function showMsg(id, text, ok) {{ const el = $(id); el.className = "msg " + (ok ? "ok" : "err"); el.textContent = text; }}

function selectedMode() {{
  const selected = document.querySelector('input[name="mode"]:checked');
  return selected ? selected.value : "oauth";
}}
function updateModeHelp() {{
  $("modehelp").textContent = selectedMode() === "setup_token"
    ? "Setup token creates a one-year, inference-only login that cannot refresh."
    : "Full OAuth creates a refreshable login that shunt manages.";
}}
for (const input of document.querySelectorAll('input[name="mode"]')) {{ input.onchange = updateModeHelp; }}

let currentName = null;
$("start").onclick = async () => {{
  const name = $("name").value.trim();
  $("addmsg").className = ""; $("addmsg").textContent = "";
  try {{
    const mode = selectedMode();
    const res = await fetch("/admin/accounts/claude", {{ method: "POST", headers: H, body: JSON.stringify({{ name, mode }}) }});
    const data = await res.json();
    if (!res.ok) {{ showMsg("addmsg", (data.error && data.error.message) || "Failed to start", false); return; }}
    currentName = data.name;
    $("authlink").textContent = data.authorize_url; $("authlink").href = data.authorize_url;
    $("step2").style.display = "block";
  }} catch (e) {{ showMsg("addmsg", "Request failed", false); }}
}};

$("complete").onclick = async () => {{
  const code = $("code").value.trim();
  try {{
    const res = await fetch("/admin/accounts/claude/" + encodeURIComponent(currentName) + "/complete",
      {{ method: "POST", headers: H, body: JSON.stringify({{ code }}) }});
    const data = await res.json();
    if (!res.ok) {{ showMsg("addmsg", (data.error && data.error.message) || "Failed to complete", false); return; }}
    showMsg("addmsg", data.message || "Account stored", true);
    $("step2").style.display = "none"; $("name").value = ""; $("code").value = "";
    loadAccounts(); loadPool();
  }} catch (e) {{ showMsg("addmsg", "Request failed", false); }}
}};

async function removeAccount(name) {{
  if (!confirm("Remove account '" + name + "'? This deletes its stored token file.")) return;
  try {{
    const res = await fetch("/admin/accounts/claude/" + encodeURIComponent(name), {{ method: "DELETE", headers: H }});
    if (!res.ok) {{ const data = await res.json().catch(() => ({{}})); showMsg("addmsg", (data.error && data.error.message) || "Failed to remove", false); return; }}
    loadAccounts(); loadPool();
  }} catch (e) {{ showMsg("addmsg", "Request failed", false); }}
}}

let currentCodexName = null;
$("start-codex").onclick = async () => {{
  const name = $("codex-name").value.trim();
  $("codex-addmsg").className = ""; $("codex-addmsg").textContent = "";
  try {{
    const res = await fetch("/admin/accounts/codex", {{ method: "POST", headers: H, body: JSON.stringify({{ name }}) }});
    const data = await res.json();
    if (!res.ok) {{ showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to start Codex login", false); return; }}
    currentCodexName = data.name;
    $("codex-authlink").textContent = data.authorize_url; $("codex-authlink").href = data.authorize_url;
    $("codex-step2").style.display = "block";
  }} catch (e) {{ showMsg("codex-addmsg", "Request failed", false); }}
}};

$("complete-codex").onclick = async () => {{
  const code = $("codex-code").value.trim();
  try {{
    const res = await fetch("/admin/accounts/codex/" + encodeURIComponent(currentCodexName) + "/complete",
      {{ method: "POST", headers: H, body: JSON.stringify({{ code }}) }});
    const data = await res.json();
    if (!res.ok) {{ showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to complete Codex login", false); return; }}
    showMsg("codex-addmsg", data.message || "Codex account stored", true);
    $("codex-step2").style.display = "none"; $("codex-name").value = ""; $("codex-code").value = "";
    loadCodexAccounts(); loadPool();
  }} catch (e) {{ showMsg("codex-addmsg", "Request failed", false); }}
}};

async function removeCodexAccount(name) {{
  if (!confirm("Remove Codex account '" + name + "'? This deletes its stored token file.")) return;
  try {{
    const res = await fetch("/admin/accounts/codex/" + encodeURIComponent(name), {{ method: "DELETE", headers: H }});
    if (!res.ok) {{ const data = await res.json().catch(() => ({{}})); showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to remove Codex account", false); return; }}
    loadCodexAccounts(); loadPool();
  }} catch (e) {{ showMsg("codex-addmsg", "Request failed", false); }}
}}

loadObserved(); loadAccounts(); loadCodexAccounts(); loadPool();
</script>
</main></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::dashboard_page;

    #[test]
    fn dashboard_is_usage_first_and_pool_management_is_collapsed() {
        let page = dashboard_page("csrf");
        let usage = page.find("<h2>Accounts and usage</h2>").unwrap();
        let management = page
            .find("<summary><strong>Manage pool accounts</strong>")
            .unwrap();
        let add_claude = page.find("<h2>Add Claude account</h2>").unwrap();
        let managed_health = page.find("<h2>Managed pool health</h2>").unwrap();
        let management_end = page[management..].find("</details>").unwrap() + management;

        assert!(usage < management);
        assert!(management < add_claude);
        assert!(add_claude < managed_health);
        assert!(managed_health < management_end);
        assert!(page.contains("Read-only signals from provider clients"));
        assert!(page.contains("read-only"));
        assert!(!page.contains("<h2>Pool health</h2>"));
    }

    #[test]
    fn observed_usage_uses_user_facing_provider_native_labels() {
        let page = dashboard_page("csrf");

        assert!(page.contains("/admin/observed"));
        assert!(page.contains("<th>Status</th><th>Usage</th>"));
        assert!(page.contains("Usage integration in progress"));
        assert!(page.contains("Waiting for traffic"));
        assert!(page.contains("Send one GPT request through this shunt"));
        assert!(page.contains("Sign in again with the provider client"));
        assert!(page.contains("role\", \"progressbar"));
        assert!(page.contains("PROVIDER_ICONS"));
        assert!(page.contains("codex: \"GPT\""));
        assert!(page.contains("untilShort"));
        assert!(!page.contains("<th>Signal</th>"));
        assert!(!page.contains("<th>Resets</th>"));
        assert!(page.contains("No supported local provider login found"));
    }
}
