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
    let script = super::script::DASHBOARD_SCRIPT.replace("{csrf}", &csrf);
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>shunt admin</title><style>{STYLE}</style></head><body><main>
<header><h1>shunt admin</h1>
<form method="post" action="/admin/logout"><button class="secondary" type="submit">Sign out</button></form>
</header>

<div id="status-section" style="display:none">
<h2>Upstream status</h2>
<p class="muted">Provider-reported status from each configured Statuspage endpoint (<code>[server.status]</code>). Observation only — never consulted by routing or failover.</p>
<div class="card overflow"><table><thead><tr><th>Provider</th><th>Status</th><th>Description</th><th>Observed</th></tr></thead>
<tbody id="status"></tbody></table></div>
</div>

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
<div class="card overflow"><table><thead><tr><th>Provider</th><th>Account</th><th>Plan</th><th>State</th><th>5h</th><th>7d</th><th>7d_oi</th><th>Status</th><th>Cooldown</th></tr></thead>
<tbody id="pool"><tr><td colspan="9" class="muted">Loading…</td></tr></tbody></table></div>
</details>

<script>
{script}
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

    #[test]
    fn claude_uuid_coalescing_is_scoped_to_the_claude_oauth_auth_kind() {
        // uuidByName is built from the Claude account store (/admin/accounts).
        // Gating on the pool account's auth kind (not its provider table's
        // display name) means a chatgpt_oauth provider named "claude" is
        // never matched against it, and a claude_oauth provider under any
        // custom name still is.
        let page = dashboard_page("csrf");
        assert!(page.contains(r#"p.auth === "claude_oauth" ? uuidByName[a.name] : null"#));
        assert!(!page.contains(r#"provider === "claude" ? uuidByName[a.name]"#));
    }

    #[test]
    fn coalesced_row_label_style_and_remediation_share_one_effective_state() {
        // Label text, the `data-state` used for CSS styling, and the
        // remediation note must all derive from the same effective state so
        // an observed override (e.g. "expired") can't show a contradictory
        // style or note computed from the un-overridden managed state.
        let page = dashboard_page("csrf");
        assert!(page.contains("function effectiveState(row)"));
        assert!(page.contains("status.dataset.state = state;"));
        assert!(!page.contains("status.dataset.state = row.state;"));
        assert!(page.contains(r#"empty.textContent = state === "expired""#));
    }

    #[test]
    fn managed_operational_states_outrank_a_stale_observed_error() {
        // A coalesced row's managed pool state (disabled/cooling/near-quota/
        // cooling-fable) is an actionable gateway-side fact and must not be
        // masked by a stale local observation error: a cooling account whose
        // last local check happened to see an expired token must still
        // surface as "cooling" (with its cooldown remediation), not "Needs
        // login" with no such hint. Guard against the precedence check being
        // introduced after -- or dropped from ahead of -- the observed-state
        // checks it must outrank.
        let page = dashboard_page("csrf");
        let start = page
            .find("function effectiveState(row)")
            .expect("effectiveState function must exist");
        let body = &page[start..start + 800];
        let guard = body
            .find(r#"row.state === "disabled" || row.state === "cooling" || row.state === "near-quota" || row.state === "cooling-fable""#)
            .expect("managed operational states must be checked before observed error states");
        let observed_expired = body
            .find(r#"o.state === "expired""#)
            .expect("observed expired check must still exist");
        assert!(
            guard < observed_expired,
            "the managed-operational-state guard must run before the observed error checks it outranks"
        );
        assert!(body[guard..].starts_with(
            r#"row.state === "disabled" || row.state === "cooling" || row.state === "near-quota" || row.state === "cooling-fable") return row.state;"#
        ));
    }

    #[test]
    fn a_fable_only_cooldown_surfaces_in_the_primary_account_state() {
        // The admin snapshot is taken with `model = None`, so a Fable-only
        // cooldown leaves `near_quota`/`available` non-Fable-scoped. Without
        // its own branch the primary account list would keep reporting such
        // an account as "Live" while Fable traffic is actually cooled -- the
        // exact conflation this milestone's Fable scoping removes. All three
        // coupled surfaces (state, label, remediation note) must carry it.
        let page = dashboard_page("csrf");
        assert!(
            page.contains(r#"a.cooldown_fable_secs_remaining ? "cooling-fable""#),
            "the primary row state must derive a Fable-only cooldown"
        );
        assert!(
            page.contains(r#"if (state === "cooling-fable") return "Cooling (Fable)";"#),
            "the Fable cooldown state needs its own label"
        );
        assert!(
            page.contains("row.managed.cooldown_fable_secs_remaining"),
            "the Fable cooldown needs its own remediation note"
        );
    }

    #[test]
    fn account_mutations_refresh_the_grouped_observed_table_too() {
        // The advanced account/pool tables (loadAccounts/loadCodexAccounts/
        // loadPool) are populated separately from the top-level grouped
        // table, which loadObserved() alone fills in. Every success path
        // that adds or removes an account must refresh loadObserved() too,
        // or the grouped table goes stale until the next full page load.
        let page = dashboard_page("csrf");
        assert_eq!(
            page.matches("loadObserved(); loadAccounts(); loadPool();")
                .count(),
            2,
            "expected both Claude add/remove success paths to refresh the grouped table"
        );
        assert_eq!(
            page.matches("loadObserved(); loadCodexAccounts(); loadPool();")
                .count(),
            2,
            "expected both Codex add/remove success paths to refresh the grouped table"
        );
    }
}
