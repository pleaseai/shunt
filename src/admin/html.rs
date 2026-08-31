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
    let script = super::script::DASHBOARD_SCRIPT
        .replace("{csrf}", &csrf)
        .replace(
            "{expiry_buffer_ms}",
            &crate::auth::claude::auth::EXPIRY_BUFFER
                .as_millis()
                .to_string(),
        );
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
<div class="card overflow"><table><thead><tr><th>Name</th><th>Kind</th><th>Status</th><th>UUID</th><th></th></tr></thead>
<tbody id="accounts"><tr><td colspan="5" class="muted">Loading…</td></tr></tbody></table></div>

<h2>Codex accounts</h2>
<div class="card overflow"><table><thead><tr><th>Name</th><th>Status</th><th>Account ID</th><th></th></tr></thead>
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
        // A coalesced row's managed pool state (disabled/needs-relogin/cooling/
        // near-quota/cooling-fable) is an actionable gateway-side fact and must
        // not be
        // masked by a stale local observation error: a cooling account whose
        // last local check happened to see an expired token must still
        // surface as "cooling" (with its cooldown remediation), not "Needs
        // login" with no such hint. `needs-relogin` joins that list for a
        // stronger reason: it is a terminal verdict the pool reached itself,
        // and a local observation must never downgrade it. Guard against the precedence check being
        // introduced after -- or dropped from ahead of -- the observed-state
        // checks it must outrank.
        let page = dashboard_page("csrf");
        let start = page
            .find("function effectiveState(row)")
            .expect("effectiveState function must exist");
        let body = &page[start..start + 800];
        let guard = body
            .find(r#"row.state === "disabled" || row.state === "needs-relogin" || row.state === "cooling" || row.state === "near-quota" || row.state === "cooling-fable""#)
            .expect("managed operational states must be checked before observed error states");
        let observed_expired = body
            .find(r#"o.state === "expired""#)
            .expect("observed expired check must still exist");
        assert!(
            guard < observed_expired,
            "the managed-operational-state guard must run before the observed error checks it outranks"
        );
        assert!(body[guard..].starts_with(
            r#"row.state === "disabled" || row.state === "needs-relogin" || row.state === "cooling" || row.state === "near-quota" || row.state === "cooling-fable") return row.state;"#
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
    fn the_claude_account_column_reports_a_kind_derived_status_not_the_raw_expiry() {
        // `expires_at` is the ~8h ACCESS-token deadline. For an `imported`
        // account shunt refreshes it in-band, so a past timestamp is routine;
        // for a `setup_token` account there is nothing to refresh, so the same
        // timestamp means a dead credential. Rendering the raw value under an
        // "Expires" header made healthy imported accounts read as expired.
        // The two kinds must therefore produce different wording, and the raw
        // timestamp must survive as a tooltip rather than being dropped.
        let page = dashboard_page("csrf");
        assert!(
            page.contains(
                "<tr><th>Name</th><th>Kind</th><th>Status</th><th>UUID</th><th></th></tr>"
            ),
            "the Claude account table must report a derived status column"
        );
        assert!(
            !page.contains(
                "<tr><th>Name</th><th>Kind</th><th>Expires</th><th>UUID</th><th></th></tr>"
            ),
            "the raw-expiry column must be gone from the Claude account table"
        );
        assert!(
            page.contains(r#"text: "Auto-refreshes""#),
            "an imported account must read as self-renewing"
        );
        assert!(
            page.contains(r#"text: "Valid until " + when(expiresAt)"#),
            "a live setup token must still surface its actionable expiry date"
        );
        assert!(
            page.contains(r#"note: "Setup token cannot refresh · re-login required""#),
            "a dead setup token must say what the operator has to do"
        );
        assert!(
            page.contains(r#"status.title = "access token expires " + when(a.expires_at)"#),
            "the raw access-token timestamp must be preserved as a tooltip"
        );
    }

    #[test]
    fn only_a_setup_token_inside_the_refresh_buffer_renders_the_expired_state() {
        // The danger styling (`.status[data-state="expired"]`) is reserved for
        // an account an operator actually has to re-provision. An imported
        // account must return before the expiry comparison is ever reached, so
        // a past `expires_at` on a healthy refreshable login can never reach
        // the expired arm; guard against the kind check being moved after --
        // or dropped from ahead of -- the timestamp branch it outranks.
        let page = dashboard_page("csrf");
        let start = page
            .find("function accountStatus(kind, expiresAt)")
            .expect("accountStatus function must exist");
        // Bound the window to the function's own closing brace rather than a
        // fixed byte count: a fixed count can land mid-character (the notes
        // carry a multi-byte "\u{b7}") and panic, or slide past the function and
        // scan unrelated helpers for the single-`expired`-arm assertion below.
        let body = &page[start..];
        let body = &body[..body.find("\n}").expect("accountStatus must be closed") + 2];
        assert!(
            body.contains(
                r#"if (kind === "imported") return { state: "available", text: "Auto-refreshes""#
            ),
            "the imported arm must return an available state before any expiry check"
        );
        let imported = body
            .find(r#"kind === "imported""#)
            .expect("the kind check must exist");
        // The comparison must carry the buffer, not the bare deadline. A setup
        // token stops being usable EXPIRY_BUFFER before its own `expiresAt`
        // (`Tokens::is_valid_at`, src/auth/claude/auth.rs) and has no refresh
        // token to recover with, so reporting it usable inside that window puts
        // the dashboard at odds with routing for the credential's last five
        // minutes -- exactly when the operator needs the warning.
        let expiry = body
            .find("expiresAt > Date.now() + EXPIRY_BUFFER_MS")
            .expect("the setup-token expiry comparison must carry the refresh buffer");
        let expired = body
            .find(r#"state: "expired""#)
            .expect("the expired state must exist");
        assert!(
            imported < expiry && expiry < expired,
            "the imported arm must precede the expiry comparison, which must precede the expired state"
        );
        assert_eq!(
            body.matches(r#"state: "expired""#).count(),
            1,
            "expired must be reachable from exactly one arm"
        );
    }

    #[test]
    fn the_rendered_refresh_buffer_is_routings_own_value() {
        // The comparison above is only correct if the number it compares
        // against is the one routing applies. `EXPIRY_BUFFER_MS` is substituted
        // from `crate::auth::claude::auth::EXPIRY_BUFFER` -- the constant
        // `Tokens::is_valid_at` uses -- so assert the value the browser
        // actually receives rather than the placeholder that produced it.
        let page = dashboard_page("csrf");
        assert!(
            !page.contains("{expiry_buffer_ms}"),
            "the buffer placeholder must be substituted, not shipped to the browser"
        );
        let rendered = format!(
            "const EXPIRY_BUFFER_MS = {};",
            crate::auth::claude::auth::EXPIRY_BUFFER.as_millis()
        );
        assert!(
            page.contains(&rendered),
            "the script must declare the buffer as routing's own value; expected `{rendered}`"
        );
        // The assertion above ties the two sides together but says nothing
        // about the unit: rendering `as_secs()` would move both. Pin the number
        // as well, so a buffer that reaches the browser a thousand times too
        // small is a failure here and not a dashboard that calls a credential
        // valid for the five minutes routing already refuses it.
        assert!(
            page.contains("const EXPIRY_BUFFER_MS = 300000;"),
            "the five-minute buffer must reach the browser as milliseconds"
        );
    }

    #[test]
    fn every_claude_account_row_offers_a_relogin_that_preselects_its_own_kind() {
        // Re-login reuses the existing add form (completing it under the same
        // name overwrites the account in place), so the row button only has to
        // prime that form. The mode must be preselected from the row's own
        // kind: re-provisioning under the other mode silently converts the
        // account between refreshable and inference-only. Remove stays.
        let page = dashboard_page("csrf");
        assert!(
            page.contains(r#"relogin.textContent = "Re-login""#),
            "each account row needs a re-login button"
        );
        assert!(
            page.contains("relogin.onclick = () => reloginAccount(a.name, a.kind);"),
            "the button must carry the row's own name and kind"
        );
        assert!(
            page.contains(
                r#"(kind === "setup_token" ? $("mode-setup") : $("mode-oauth")).checked = true;"#
            ),
            "the add form's login method must be preselected from the row's kind"
        );
        assert!(
            page.contains(r#"$("name").value = name;"#),
            "re-login must prefill the add form with the existing account name"
        );
        assert!(
            page.contains(r#"btn.onclick = () => removeAccount(a.name);"#),
            "the existing Remove action must survive"
        );
        assert!(
            page.contains("  currentName = null;"),
            "priming the form must drop the previously started flow's account handle, \
             not just hide the step that used it"
        );
    }

    #[test]
    fn the_codex_account_column_reports_renewal_ownership_not_the_raw_expiry() {
        // Same defect as the Claude table, with a simpler resolution: the Codex
        // store has no non-refreshable kind at all. Both writers reject a
        // missing or empty refresh token (`import_auth` and
        // `store_chatgpt_tokens`), so shunt owns renewal for every row and the
        // access-token JWT `exp` this column printed is never actionable --
        // it just made every healthy account read as broken within the hour.
        // The status is therefore unconditional, and must not be re-derived
        // from the timestamp; the timestamp survives only as the tooltip.
        let page = dashboard_page("csrf");
        assert!(
            page.contains("<tr><th>Name</th><th>Status</th><th>Account ID</th><th></th></tr>"),
            "the Codex account table must report renewal ownership, not a raw expiry"
        );
        assert!(
            !page.contains("<tr><th>Name</th><th>Expires</th><th>Account ID</th><th></th></tr>"),
            "the raw-expiry column must be gone from the Codex account table"
        );
        let start = page
            .find("async function loadCodexAccounts()")
            .expect("loadCodexAccounts must exist");
        let body = &page[start..];
        let body = &body[..body.find("\n}").expect("loadCodexAccounts must be closed") + 2];
        assert!(
            body.contains(r#"const status = cell(r, "Auto-refreshes"); status.className = "status"; status.dataset.state = "available";"#),
            "every Codex row must state that shunt renews it"
        );
        assert!(
            body.contains(r#"statusNote.textContent = "shunt renews this login as needed""#),
            "the Codex status needs the same remediation-free note as an imported Claude row"
        );
        assert!(
            body.contains(r#"status.title = "access token expires " + when(a.expires_at)"#),
            "the raw access-token timestamp must be preserved as a tooltip"
        );
        assert!(
            !body.contains("cell(r, when(a.expires_at))"),
            "the Codex row must not render the raw access-token expiry as a column again"
        );
    }

    #[test]
    fn every_codex_account_row_offers_a_relogin_that_clears_the_prior_flow() {
        // Symmetric with the Claude table, and safe for the same reason: the
        // Codex start route has no duplicate-name guard and completion
        // overwrites in place, cleaning up the replaced identity's pool health.
        // There is no login method to preselect (ChatGPT OAuth is the only way
        // into this store), so the whole contract is: prime the name, and drop
        // the half-finished flow -- `currentCodexName` included, since that is
        // the handle the completion POST interpolates into its URL.
        let page = dashboard_page("csrf");
        let start = page
            .find("function reloginCodexAccount(name)")
            .expect("reloginCodexAccount must exist");
        let body = &page[start..];
        let body = &body[..body
            .find("\n}")
            .expect("reloginCodexAccount must be closed")
            + 2];
        assert!(
            page.contains("relogin.onclick = () => reloginCodexAccount(a.name);"),
            "each Codex row needs a re-login button carrying its own name"
        );
        assert!(
            body.contains(r#"$("codex-name").value = name;"#),
            "re-login must prefill the Codex add form with the existing account name"
        );
        assert!(
            body.contains("  currentCodexName = null;"),
            "priming the form must drop the previously started flow's account handle"
        );
        assert!(
            body.contains(
                r#"$("codex-step2").style.display = "none"; $("codex-code").value = "";"#
            ),
            "the half-finished Codex flow must be cleared, not left pasteable"
        );
        assert!(
            page.contains(r#"btn.onclick = () => removeCodexAccount(a.name);"#),
            "the existing Codex Remove action must survive"
        );
    }

    #[test]
    fn a_superseded_provisioning_response_cannot_restore_a_cleared_flow() {
        // Clearing the form on Re-login is not enough on its own: a start or
        // completion request already in flight writes its result back
        // unconditionally when it lands. A late start response would reopen the
        // previous account's authorization step and restore its `currentName`
        // while the name field already reads the newly picked account -- so
        // following that reopened link stores the freshly authorized credential
        // under the OLD account's name, silently overwriting a different pool
        // account. A late completion would blank the just-primed name and
        // report success for the wrong account.
        //
        // Assert per handler body rather than by counting matches across the
        // page: a page-wide count is satisfied by deleting a guard from one
        // handler and duplicating it in another, which leaves one of the four
        // response paths writing back after being superseded.
        let page = dashboard_page("csrf");
        for (counter, handler) in [
            ("claudeFlowEpoch", r#"$("start").onclick = async () => {"#),
            (
                "claudeFlowEpoch",
                r#"$("complete").onclick = async () => {"#,
            ),
            (
                "codexFlowEpoch",
                r#"$("start-codex").onclick = async () => {"#,
            ),
            (
                "codexFlowEpoch",
                r#"$("complete-codex").onclick = async () => {"#,
            ),
        ] {
            let at = page.find(handler).expect("the handler must exist");
            let body = &page[at..];
            let body = &body[..body.find("\n};").expect("handler must be closed") + 3];
            assert_eq!(
                body.matches(&format!("const epoch = ++{counter};")).count(),
                1,
                "{handler} must capture the epoch it was issued under, exactly once"
            );
            assert_eq!(
                body.matches(&format!("if (epoch !== {counter}) return;"))
                    .count(),
                1,
                "{handler} must discard its own response once superseded"
            );
            // Asserted by shape rather than by one literal line: every report
            // the failure path makes must sit behind the same guard, however
            // that block is laid out. An unguarded showMsg there would speak
            // for a flow the operator has already abandoned.
            let caught = body
                .find("} catch (e)")
                .expect("the handler must have a failure path");
            let failure = &body[caught..];
            let failure = &failure[..failure.find("\n  finally {").unwrap_or(failure.len())];
            let reports = failure.matches("showMsg(").count();
            assert!(
                reports > 0
                    && reports
                        == failure
                            .matches(&format!("if (epoch === {counter}) showMsg("))
                            .count(),
                "{handler} must stay silent on failure once superseded"
            );
        }

        // Each form counts on its own, so re-priming one never discards the
        // other's live flow, and each primer supersedes what is in flight.
        for (counter, primer) in [
            ("claudeFlowEpoch", "function reloginAccount(name, kind)"),
            ("codexFlowEpoch", "function reloginCodexAccount(name)"),
        ] {
            assert!(
                page.contains(&format!("let {counter} = 0;")),
                "{counter} must exist as its own per-form counter"
            );
            let at = page.find(primer).expect("the primer function must exist");
            let body = &page[at..];
            let body = &body[..body.find("\n}").expect("primer must be closed") + 2];
            assert!(
                body.contains(&format!("  {counter}++;")),
                "{primer} must supersede any request still in flight"
            );
        }

        // A completion that reached the server stored the account whether or
        // not its flow was superseded, so the table refresh must run before the
        // guard -- only the confirmation and the form reset are gated.
        for (counter, handler, refresh) in [
            (
                "claudeFlowEpoch",
                r#"$("complete").onclick = async () => {"#,
                "loadObserved(); loadAccounts(); loadPool();",
            ),
            (
                "codexFlowEpoch",
                r#"$("complete-codex").onclick = async () => {"#,
                "loadObserved(); loadCodexAccounts(); loadPool();",
            ),
        ] {
            let at = page.find(handler).expect("the handler must exist");
            let body = &page[at..];
            let body = &body[..body.find("\n};").expect("handler must be closed") + 3];
            let refreshed = body
                .find(refresh)
                .expect("a completed account must refresh the tables");
            let gate = body
                .find(&format!("if (epoch !== {counter}) return;"))
                .expect("the guard must exist");
            assert!(
                refreshed < gate,
                "{handler} must refresh the tables before returning on a superseded flow"
            );
        }
    }

    #[test]
    fn a_second_completion_click_cannot_supersede_the_one_that_stores_the_account() {
        // A completion is the one request in the flow that consumes the pending
        // login, so the FIRST click is the one that stores the credential and a
        // second finds the entry already consumed and fails. Letting that second
        // click bump the flow epoch inverts which response the operator sees:
        // the successful one is silenced by the epoch guard the confirmation and
        // the form reset sit behind, while the failed one reports an error over
        // an account that was in fact stored, leaving the finished form open.
        // Starts are the opposite -- there the later click is the live one -- so
        // this is asserted on the completion handlers alone.
        let page = dashboard_page("csrf");
        for (flag, button, handler) in [
            (
                "claudeCompleting",
                "complete",
                r#"$("complete").onclick = async () => {"#,
            ),
            (
                "codexCompleting",
                "complete-codex",
                r#"$("complete-codex").onclick = async () => {"#,
            ),
        ] {
            assert_eq!(
                page.matches(&format!("let {flag} = false;")).count(),
                1,
                "{flag} must exist as its own per-form in-flight marker"
            );
            let at = page.find(handler).expect("the handler must exist");
            let body = &page[at..];
            let body = &body[..body.find("\n};").expect("handler must be closed") + 3];
            let refused = body
                .find(&format!("if ({flag}) return;"))
                .expect("a second completion click must be refused");
            let raised = body
                .find(&format!("{flag} = true;"))
                .expect("the completion must mark itself in flight");
            let bumped = body
                .find("const epoch = ++")
                .expect("the completion must capture its epoch");
            assert!(
                refused < raised && raised < bumped,
                "{handler} must refuse the second click before it can bump the epoch"
            );
            assert!(
                body.contains(&format!(
                    "finally {{ clearTimeout(bound); {flag} = false; $(\"{button}\").disabled = false; }}"
                )),
                "{handler} must release the marker on every exit, not only on success"
            );
            // The marker must not be released by a newer start or a re-login
            // either, tempting as that is for a page whose Complete button is
            // closed: that would put two completions in flight against different
            // pending entries, and the server does not order them --
            // `PendingStore::attempt` leaves the entry in place and
            // `complete_account` removes it only after the store, so the older
            // exchange can land last and leave the account holding the superseded
            // credential. The request carries its own bound instead, so a
            // connection that never settles cannot close the button for good.
            assert_eq!(
                page.matches(&format!("{flag} = false")).count(),
                2,
                "{flag} must be released in exactly one place -- its declaration and the handler's finally"
            );
            assert!(
                body.contains("const abort = new AbortController();")
                    && body.contains("setTimeout(() => abort.abort(), COMPLETE_TIMEOUT_MS);")
                    && body.contains("signal: abort.signal"),
                "{handler} must bound its own request so the marker cannot strand"
            );
            // An abandoned completion is ambiguous -- the exchange may have
            // stored the account before the page stopped waiting -- so the
            // failure path must re-read the server rather than leave the
            // operator to guess the outcome from an error message.
            let abandoned = body
                .find("} catch (e) {")
                .expect("the handler must have a failure path");
            let refreshed = body[abandoned..]
                .find("loadObserved(")
                .expect("an abandoned completion must refresh the tables");
            let reported = body[abandoned..]
                .find("showMsg(")
                .expect("an abandoned completion must say so");
            assert!(
                refreshed < reported,
                "{handler} must refresh the tables before reporting an abandoned completion"
            );
            assert!(
                body.contains(&format!("$(\"{button}\").disabled = true;")),
                "{handler} must also close the button so the refusal is visible"
            );
        }
    }

    #[test]
    fn account_mutations_refresh_the_grouped_observed_table_too() {
        // The advanced account/pool tables (loadAccounts/loadCodexAccounts/
        // loadPool) are populated separately from the top-level grouped
        // table, which loadObserved() alone fills in. Every path that mutates
        // an account must refresh loadObserved() too, or the grouped table
        // goes stale until the next full page load.
        //
        // Asserted per mutation site rather than by counting the call across
        // the page: a completion's abandoned path refreshes the tables too (an
        // abandoned request leaves the outcome unknown), so a page-wide count
        // no longer distinguishes "every mutating path refreshes" from "some
        // path somewhere does".
        let page = dashboard_page("csrf");
        for (site, refresh) in [
            (
                r#"$("complete").onclick = async () => {"#,
                "loadObserved(); loadAccounts(); loadPool();",
            ),
            (
                "async function removeAccount(name) {",
                "loadObserved(); loadAccounts(); loadPool();",
            ),
            (
                r#"$("complete-codex").onclick = async () => {"#,
                "loadObserved(); loadCodexAccounts(); loadPool();",
            ),
            (
                "async function removeCodexAccount(name) {",
                "loadObserved(); loadCodexAccounts(); loadPool();",
            ),
        ] {
            let at = page.find(site).expect("the mutation site must exist");
            let body = &page[at..];
            let body = &body[..body.find("\n}").expect("the site must be closed") + 2];
            // Bounded to the success branch. A completion also refreshes on its
            // abandoned path, so a whole-body `contains` would still pass with
            // the success-path refresh deleted -- the very regression this test
            // exists to catch. Exactly once, so a duplicate is caught too.
            let success = &body[..body.find("} catch (e)").unwrap_or(body.len())];
            assert_eq!(
                success.matches(refresh).count(),
                1,
                "{site} must refresh the grouped observed table exactly once on success"
            );
        }

        // The refresh probe is the exception, and deliberately so: *both* of
        // its branches refresh, because a terminal verdict sets
        // `needs_relogin` and the grouped table renders it — there the failure
        // is exactly the state change worth showing. Asserted on the whole
        // function body rather than the success branch alone.
        let at = page
            .find("async function refreshAccount(name) {")
            .expect("the refresh probe must exist");
        let body = &page[at..];
        let body = &body[..body.find("\n}").expect("the probe must be closed") + 2];
        assert_eq!(
            body.matches("loadObserved(); loadAccounts(); loadPool();")
                .count(),
            2,
            "both branches of the refresh probe must refresh the grouped table"
        );
    }
}
