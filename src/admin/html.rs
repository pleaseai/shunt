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
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body { font-family: system-ui, -apple-system, Segoe UI, Roboto, sans-serif; margin: 0;
  background: #f6f7f9; color: #1a1a1a; }
@media (prefers-color-scheme: dark) { body { background: #16181d; color: #e6e6e6; } }
main { max-width: 60rem; margin: 0 auto; padding: 1.5rem 1rem 4rem; }
h1 { font-size: 1.3rem; } h2 { font-size: 1.05rem; margin-top: 2rem; }
header { display: flex; align-items: center; justify-content: space-between; }
.card { background: canvas; border: 1px solid #8884; border-radius: 10px; padding: 1rem 1.1rem;
  margin-top: 1rem; }
label { display: block; font-size: .85rem; margin: .5rem 0 .2rem; }
input, textarea, button { font: inherit; }
input, textarea { width: 100%; padding: .5rem .6rem; border: 1px solid #8886; border-radius: 8px;
  background: canvas; color: inherit; }
@media (max-width: 40rem) { input, textarea { font-size: 1rem; } }
fieldset { border: 0; padding: 0; margin: .7rem 0; }
legend { font-size: .85rem; margin-bottom: .25rem; }
.choice { display: flex; gap: .45rem; align-items: flex-start; margin: .25rem 0; padding: .2rem 0; }
.choice input { flex: 0 0 auto; width: auto; margin: .2rem 0 0; }
.choice span { display: block; }
.choice small { display: block; margin-top: .1rem; }
textarea { min-height: 4.5rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
button { min-height: 2.75rem; touch-action: manipulation; cursor: pointer; padding: .5rem .9rem;
  border: 1px solid #4661ff; border-radius: 8px; background: #4661ff; color: #fff; }
button:focus-visible, input:focus-visible, textarea:focus-visible, .choice:has(input:focus-visible) { outline: 2px solid #4661ff; outline-offset: 2px; }
button.secondary { background: transparent; color: inherit; border-color: #8886; }
button.danger { background: transparent; color: #c0392b; border-color: #c0392b88; padding: .25rem .5rem; }
table { width: 100%; border-collapse: collapse; margin-top: .5rem; font-size: .9rem; }
th, td { text-align: left; padding: .4rem .5rem; border-bottom: 1px solid #8883; }
th { font-weight: 600; opacity: .8; }
code, .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85em; }
.msg { padding: .6rem .8rem; border-radius: 8px; margin-top: .6rem; font-size: .9rem; }
.msg.err { background: #c0392b22; } .msg.ok { background: #27ae6022; }
.muted { opacity: .65; } .row { display: flex; gap: .6rem; align-items: end; }
.overflow { overflow-x: auto; }
a { color: #4661ff; }
"#;

/// The login form. `error` is shown above the form when a prior attempt failed.
pub fn login_page(error: Option<&str>) -> String {
    let error_block = match error {
        Some(message) => format!(r#"<div class="msg err">{}</div>"#, escape_html(message)),
        None => String::new(),
    };
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

<h2>Pool health</h2>
<div class="card overflow"><table><thead><tr><th>Provider</th><th>Account</th><th>State</th><th>5h</th><th>7d</th><th>7d_oi</th><th>Status</th><th>Cooldown</th></tr></thead>
<tbody id="pool"><tr><td colspan="8" class="muted">Loading…</td></tr></tbody></table></div>

<script>
const CSRF = "{csrf}";
const H = {{ "content-type": "application/json", "x-csrf-token": CSRF }};
const $ = (id) => document.getElementById(id);
function esc(v) {{ return v === null || v === undefined ? "" : String(v); }}
function pct(v) {{ return v === null || v === undefined ? "—" : Math.round(v * 100) + "%"; }}
function when(ms) {{ return ms ? new Date(ms).toLocaleString() : "—"; }}
function cell(row, text, mono) {{ const td = document.createElement("td"); td.textContent = esc(text);
  if (mono) td.className = "mono"; row.appendChild(td); return td; }}

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
    cell(r, pct(a.utilization_5h)); cell(r, pct(a.utilization_7d)); cell(r, pct(a.utilization_7d_oi));
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

loadAccounts(); loadCodexAccounts(); loadPool();
</script>
</main></body></html>"#
    )
}
