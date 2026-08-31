//! The admin dashboard's inline script, split out of `html.rs` to keep that
//! file within the repository's per-file size guidance. Held as a plain const
//! rather than a `format!` argument so the JavaScript reads as JavaScript:
//! inside a format string every brace has to be doubled. The single `{csrf}`
//! placeholder is substituted by `html::dashboard_page`.
//!
//! As in `html.rs`, all account/pool data is written with `textContent` and
//! never `innerHTML`, so upstream-derived strings cannot inject markup.

pub(super) const DASHBOARD_SCRIPT: &str = r#"const CSRF = "{csrf}";
const H = { "content-type": "application/json", "x-csrf-token": CSRF };
const $ = (id) => document.getElementById(id);
function esc(v) { return v === null || v === undefined ? "" : String(v); }
function pct(v) { return v === null || v === undefined ? "—" : Math.round(v * 100) + "%"; }
// Mirrors the Rust `title_case` helper (src/auth/observation.rs): capitalize
// only the first character, matching the wording the observed-accounts view
// already shows for a plan (e.g. "Max plan"), not per-word title casing.
function titleCase(v) { return v ? v.charAt(0).toUpperCase() + v.slice(1) : ""; }
function untilShort(resetSecs) {
  const mins = Math.ceil((resetSecs * 1000 - Math.min(Date.now(), resetSecs * 1000)) / 60000);
  if (mins <= 0) return "now";
  const d = Math.floor(mins / 1440), h = Math.floor((mins % 1440) / 60), m = mins % 60;
  return d > 0 ? (h > 0 ? d + "d " + h + "h" : d + "d") : h > 0 ? (m > 0 ? h + "h " + m + "m" : h + "h") : m + "m";
}
function pctReset(v, resetSecs) {
  return resetSecs ? pct(v) + " · " + untilShort(resetSecs) : pct(v);
}
function when(ms) { return ms ? new Date(ms).toLocaleString() : "—"; }
function cell(row, text, mono) { const td = document.createElement("td"); td.textContent = esc(text);
  if (mono) td.className = "mono"; row.appendChild(td); return td; }

function providerLabel(provider) { return ({ claude: "Claude", codex: "GPT", grok: "Grok", kimi: "Kimi", gemini: "Gemini", cursor: "Cursor" })[provider] || provider; }
const PROVIDER_ICONS = {
  claude: ["0 0 24 24", "m4.714 15.956 4.718-2.648.079-.23-.079-.128H9.2l-6.866-.34-1.142-.243-.534-.704.055-.352.48-.322 8.116.558.158-.158-7.068-4.91-.722-.492-.364-.461-.158-1.008.656-.722.88.06 8.5 6.338.158-.073-3.825-7.073-.17-.62c-.061-.255-.104-.467-.104-.728L6.287.134 6.7 0l.996.134.419.364 3.868 8.402.158.255h.158l.364-7.255.377-.91.747-.492.583.28.48.685-1.275 7.012h.212l4.44-5.135.85-.905h1.032l.759 1.13-.34 1.165-4.845 6.547.073.11 6.266-1.202.832.389.091.394-.328.807-7.534 1.76.049.061 5.849.407.789.522.474.638-.079.486-1.214.619-5.318-1.226h-.182v.109l6.854 6.22.127.577-.321.455-.34-.048-5.747-4.768h-.128v.17l2.908 4.171.121 1.081-.17.352-.607.213-.668-.122-3.369-4.808-1.141-1.943-.14.079-.674 7.255-.315.37-.729.28-.607-.462-.322-.747 1.603-7.023-.012-.043-.14.019-5.336 7.322-.413.164-.716-.37.067-.662.4-.589 4.754-6.004-.006-.158h-.055l-6.338 4.117-1.13.145-.485-.455.06-.747.231-.243Z"],
  codex: ["0 0 24 24", "M22.282 9.821a5.985 5.985 0 0 0-.516-4.911 6.046 6.046 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.182a5.985 5.985 0 0 0-3.998 2.9 6.046 6.046 0 0 0 .743 7.096 5.98 5.98 0 0 0 .511 4.911 6.051 6.051 0 0 0 6.515 2.9A5.985 5.985 0 0 0 13.26 24a6.056 6.056 0 0 0 5.772-4.206 5.989 5.989 0 0 0 3.997-2.9 6.056 6.056 0 0 0-.747-7.073Zm-9.022 12.608a4.476 4.476 0 0 1-2.877-1.041l4.92-2.839a.795.795 0 0 0 .393-.68v-6.737l2.02 1.168.038.052v5.583a4.504 4.504 0 0 1-4.494 4.494Zm-9.661-4.125a4.471 4.471 0 0 1-.535-3.014l4.925 2.843a.771.771 0 0 0 .781 0l5.843-3.369v2.333l-.033.061-4.84 2.792a4.499 4.499 0 0 1-6.141-1.646ZM2.341 7.896a4.485 4.485 0 0 1 2.365-1.973V11.6a.766.766 0 0 0 .388.677l5.814 3.354-2.02 1.169h-.071l-4.83-2.787a4.504 4.504 0 0 1-1.646-6.141Zm16.596 3.856-5.833-3.388L15.119 7.2h.071l4.83 2.791a4.494 4.494 0 0 1-.676 8.104v-5.677a.79.79 0 0 0-.407-.667Zm2.011-3.024-4.916-2.867a.776.776 0 0 0-.785 0L9.409 9.23V6.897l.028-.061 4.83-2.787a4.499 4.499 0 0 1 6.681 4.66ZM8.307 12.863l-2.02-1.164-.038-.057V6.074a4.499 4.499 0 0 1 7.376-3.453L8.704 5.459a.795.795 0 0 0-.393.682Zm1.097-2.365 2.602-1.5 2.607 1.5v2.999l-2.597 1.5-2.607-1.5Z"],
  gemini: ["0 0 24 24", "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81"],
  kimi: ["0 0 24 24", "m1.053 16.91 9.538 2.55q-.03 1.02.06 2.031l5.956 1.592A12 11.99 0 0 1 1.053 16.91M.033 11.12l11.352 3.036q-.3.99-.469 2.01l10.817 2.89a12 11.99 0 0 1-1.845 2.004L.658 15.918a12 11.99 0 0 1-.625-4.796m1.593-5.146L13.573 9.17q-.57.9-1.01 1.874l11.297 3.02q-.24 1.2-.67 2.362L.125 10.26a12 11.99 0 0 1 1.5-4.285ZM6.067 1.58l11.285 3.016q-.9.78-1.688 1.719l7.824 2.091q.42 1.29.513 2.664L2.107 5.218a12 11.99 0 0 1 3.96-3.638M21.68 4.866 7.222 1.003A12 11.99 0 0 1 21.68 4.866"],
  cursor: ["0 0 24 24", "M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23"],
  grok: ["0 0 24 24", "M7.75 14.66 14 9.85c.3-.24.75-.15.89.22a5.4 5.4 0 0 1-6.71 7.01l-2.12 1.02a6.9 6.9 0 0 0 11.25-7.63c-.77-3.45.19-4.83 2.15-7.65l.54-.78-2.58 2.69L7.75 14.66Zm-1.29 1.17a5.4 5.4 0 0 1 .06-7.48 5.45 5.45 0 0 1 5.6-1.16l2.11-1.02a6.9 6.9 0 0 0-9.05 11.68c.8 2.03-.51 3.46-1.83 4.91-.47.51-.94 1.03-1.31 1.57l4.42-4.12Z"]
};
function providerCell(row, provider) {
  const td = document.createElement("td"), wrap = document.createElement("span"); wrap.className = "provider";
  const icon = PROVIDER_ICONS[provider];
  if (icon) { const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg"); svg.classList.add("provider-logo");
    svg.setAttribute("viewBox", icon[0]); svg.setAttribute("aria-hidden", "true");
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path"); path.setAttribute("d", icon[1]); path.setAttribute("fill", "currentColor"); svg.appendChild(path); wrap.appendChild(svg); }
  const label = document.createElement("span"); label.textContent = providerLabel(provider); wrap.appendChild(label); td.appendChild(wrap); row.appendChild(td);
}
function usageBar(parent, label, remaining, resetTime) {
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
}

// Pool providers are config-named (`[providers.anthropic]`); observations are
// vendor-named. Only the built-in kind is mapped — a provider table under any
// other name simply renders as its own group rather than being mis-merged.
const POOL_PROVIDER_ALIASES = { anthropic: "claude" };

// Fold managed pool accounts and read-only observations into one row set per
// provider. Coalesced by account uuid: when a managed account holds the same
// subscription as the local client login, that is ONE account seen through two
// lenses, not two accounts. Listing it twice is precisely what made this table
// unreadable with a pool configured.
function accountGroups(observed, pool, accounts) {
  const uuidByName = {};
  for (const a of ((accounts && accounts.accounts) || [])) if (a.uuid) uuidByName[a.name] = a.uuid;
  const groups = new Map();
  const groupFor = (provider) => { if (!groups.has(provider)) groups.set(provider, []); return groups.get(provider); };
  const byUuid = new Map();
  for (const p of ((pool && pool.providers) || [])) {
    const provider = POOL_PROVIDER_ALIASES[p.provider] || p.provider;
    for (const a of (p.accounts || [])) {
      // A managed-only row (no observed override below) shows the pool's own
      // plan as its detail caption, matching the wording the observed path
      // already uses ("Max plan") -- see observation::parse_claude/parse_codex.
      const row = { provider: provider, label: a.name, detail: a.plan ? titleCase(a.plan) + " plan" : null, managed: a, observed: null,
        // `needs_relogin` is checked before the cooldown states for the same
        // reason the pool table checks it first: a dead credential is *also*
        // cooling, and this is the primary table operators actually read.
        state: a.disabled ? "disabled" : a.needs_relogin ? "needs-relogin" : !a.has_state ? "unseen" : a.cooldown_secs_remaining ? "cooling" : a.near_quota ? "near-quota" : a.cooldown_fable_secs_remaining ? "cooling-fable" : "available",
        utilization_5h: a.utilization_5h, reset_5h: a.reset_5h,
        utilization_7d: a.utilization_7d, reset_7d: a.reset_7d,
        utilization_7d_oi: a.utilization_7d_oi, reset_7d_oi: a.reset_7d_oi };
      groupFor(provider).push(row);
      // uuidByName is sourced from the Claude account store only (see
      // /admin/accounts), so only claude_oauth accounts may be matched
      // against it. Gate on the account's actual auth kind (p.auth), not the
      // provider's display name/group key: a provider table can be named
      // anything, so a chatgpt_oauth provider named "claude" would otherwise
      // still get Claude uuids applied, and a claude_oauth provider under a
      // custom name would otherwise never get them.
      const uuid = p.auth === "claude_oauth" ? uuidByName[a.name] : null;
      if (uuid) byUuid.set(uuid, row);
    }
  }
  for (const o of observed) {
    // A missing uuid means "identity unknown" and must never match.
    const match = o.uuid ? byUuid.get(o.uuid) : null;
    if (match) {
      match.observed = o;
      // Prefer the observed detail when present; otherwise keep the
      // plan-derived detail from the pool payload. observed comes from
      // ~/.claude/.credentials.json while the pool's detail comes from the
      // account's store file or the live profile API -- different sources,
      // and a null observed detail must not blank out a real plan-derived one.
      if (o.detail) match.detail = o.detail;
      // Prefer the client's windows: the pool only learns a window from a
      // response header it has actually received, so it reports null for
      // windows the client can already see.
      for (const k of ["utilization_5h", "reset_5h", "utilization_7d", "reset_7d", "utilization_7d_oi", "reset_7d_oi"])
        if (o[k] !== null && o[k] !== undefined) match[k] = o[k];
      continue;
    }
    groupFor(o.provider).push({ provider: o.provider, label: o.identity || o.provider, detail: o.detail,
      managed: null, observed: o, state: o.state,
      utilization_5h: o.utilization_5h, reset_5h: o.reset_5h,
      utilization_7d: o.utilization_7d, reset_7d: o.reset_7d,
      utilization_7d_oi: o.utilization_7d_oi, reset_7d_oi: o.reset_7d_oi });
  }
  return groups;
}

// Coalescing lets an observed row override a managed row's displayed status
// (e.g. the client sees "expired" quota the pool has not detected yet). The
// label, the `data-state` used for styling, and the remediation note must all
// read this same effective state -- otherwise they can disagree, e.g. a
// "Needs login" label next to a green "available" dot with no login hint.
function effectiveState(row) {
  // Managed pool operational states are actionable gateway-side facts (the
  // pool disabled the account, is cooling it down -- account-wide or for
  // Fable only -- or sees it near quota) and must win over a stale local
  // observation error: an account the pool has already benched should not
  // read "Needs login" -- with no cooldown remediation -- just because the
  // last local check happened to see an expired token.
  if (row.state === "disabled" || row.state === "needs-relogin" || row.state === "cooling" || row.state === "near-quota" || row.state === "cooling-fable") return row.state;
  const o = row.observed;
  if (o) {
    if (o.state === "expired") return "expired";
    if (o.state === "unavailable") return "unavailable";
    if (o.state === "waiting-for-traffic") return "waiting-for-traffic";
    if (o.signal === "integration-pending") return "connected";
  }
  return row.state;
}

function rowStatusText(state) {
  if (state === "needs-relogin") return "Needs re-login";
  if (state === "expired") return "Needs login";
  if (state === "unavailable") return "Usage unavailable";
  if (state === "waiting-for-traffic") return "Waiting for traffic";
  if (state === "connected") return "Connected";
  if (state === "disabled") return "Disabled";
  if (state === "cooling") return "Cooling";
  if (state === "cooling-fable") return "Cooling (Fable)";
  if (state === "near-quota") return "Near quota";
  if (state === "unseen") return "No traffic yet";
  return "Live";
}

async function loadObserved() {
  const body = $("observed"); body.textContent = "";
  let data, res;
  try { res = await fetch("/admin/observed"); data = await res.json(); }
  catch (e) { const r = body.insertRow(); const c = cell(r, "Failed to observe local accounts"); c.colSpan = 4; return; }
  if (!res.ok) { const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to observe local accounts"); c.colSpan = 4; return; }
  // Managed pool state only enriches this view. If either call fails the table
  // still renders the observations alone rather than rendering nothing.
  let pool = null, accounts = null;
  try {
    // Per-fetch catch, not one around Promise.all: a transient failure on one
    // endpoint must not discard the other's result.
    const [poolRes, accountsRes] = await Promise.all([
      fetch("/admin/pool").catch(() => null),
      fetch("/admin/accounts").catch(() => null)
    ]);
    if (poolRes && poolRes.ok) pool = await poolRes.json();
    if (accountsRes && accountsRes.ok) accounts = await accountsRes.json();
  } catch (e) { /* observation-only render */ }
  const groups = accountGroups((data && data.accounts) || [], pool, accounts);
  let rendered = 0;
  for (const [provider, rows] of groups) {
    rows.forEach((row, index) => {
      rendered++;
      const r = body.insertRow();
      // The provider is named once per group; continuation rows keep the grid
      // column so the account labels stay aligned under it.
      if (index === 0) providerCell(r, provider);
      else { const spacer = document.createElement("td"); spacer.className = "provider-continued"; r.appendChild(spacer); }
      const identity = cell(r, row.label);
      if (row.detail) { const detail = document.createElement("small"); detail.className = "account-detail"; detail.textContent = row.detail; identity.appendChild(detail); }
      if (row.managed && row.observed) identity.title = "managed pool account · same subscription as the local " + providerLabel(provider) + " login";
      else if (row.managed) identity.title = "managed pool account";
      else if (row.observed) identity.title = row.observed.source + " · read-only";
      const state = effectiveState(row);
      const pending = state === "connected";
      if (pending) r.className = "pending-row";
      const status = cell(r, rowStatusText(state)); status.className = "status"; status.dataset.state = state;
      const statusNote = document.createElement("small"); statusNote.className = "status-note";
      if (state === "waiting-for-traffic") statusNote.textContent = "Quota arrives in GPT response headers";
      else if (state === "expired") statusNote.textContent = "The provider client owns refresh";
      else if (state === "unavailable") statusNote.textContent = "Current login could not read quota";
      else if (state === "needs-relogin") statusNote.textContent = "Re-add this account to sign in again";
      else if (row.managed && row.managed.cooldown_secs_remaining) statusNote.textContent = "retries in " + untilShort(Math.floor(Date.now() / 1000) + row.managed.cooldown_secs_remaining);
      else if (row.managed && row.managed.cooldown_fable_secs_remaining) statusNote.textContent = "Fable retries in " + untilShort(Math.floor(Date.now() / 1000) + row.managed.cooldown_fable_secs_remaining);
      if (statusNote.textContent) status.appendChild(statusNote);
      if (row.observed && row.observed.message) status.title = row.observed.message;
      const usage = document.createElement("td"); usage.className = "usage-lines"; r.appendChild(usage);
      const buckets = ((row.observed && row.observed.quota_buckets) || []).filter(b => b.remaining !== null && b.remaining !== undefined);
      if (buckets.length) {
        for (const b of buckets) usageBar(usage, b.label, b.remaining, b.reset_time);
        usage.title = buckets.map(b => b.label + ": " + Math.round((1 - b.remaining) * 1000) / 10 + "% used" + (b.reset_time ? ", resets " + new Date(b.reset_time).toLocaleString() : "")).join("\n");
      } else {
        const windows = [
          ["5h", row.utilization_5h, row.reset_5h],
          ["Week", row.utilization_7d, row.reset_7d],
          ["Fable", row.utilization_7d_oi, row.reset_7d_oi]
        ].filter(window => window[1] !== null && window[1] !== undefined);
        for (const window of windows) usageBar(usage, window[0], 1 - window[1], window[2] ? new Date(window[2] * 1000).toISOString() : null);
        if (!windows.length) { const empty = document.createElement("span"); empty.className = "usage-empty";
          empty.textContent = state === "expired" ? "Sign in again with the provider client"
            : state === "waiting-for-traffic" ? "Send one GPT request through this shunt"
            : pending ? "Usage integration in progress"
            : "No usage reported yet"; usage.appendChild(empty); }
      }
    });
  }
  if (!rendered) { const r = body.insertRow(); const c = cell(r, "No supported local provider login found. Sign in with a provider CLI."); c.colSpan = 4; c.className = "muted"; }
}

async function loadAccounts() {
  const body = $("accounts"); body.textContent = "";
  let data, res;
  try { res = await fetch("/admin/accounts"); data = await res.json(); }
  catch (e) { const r = body.insertRow(); const c = cell(r, "Failed to load accounts"); c.colSpan = 5; return; }
  if (!res.ok) { const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to load accounts"); c.colSpan = 5; return; }
  const list = (data && data.accounts) || [];
  if (!list.length) { const r = body.insertRow(); const c = cell(r, "No store accounts yet"); c.colSpan = 5; c.className = "muted"; return; }
  for (const a of list) {
    const r = body.insertRow();
    cell(r, a.name); cell(r, a.kind); cell(r, when(a.expires_at)); cell(r, a.uuid || "—", true);
    const td = document.createElement("td");
    // Only an imported login carries a refresh grant; a setup-token account has
    // nothing to probe (the endpoint refuses it), so it gets no button.
    if (a.kind === "imported") {
      const refresh = document.createElement("button"); refresh.className = "secondary"; refresh.textContent = "Refresh";
      refresh.title = "Exercise this account's refresh grant now and report whether the login is still alive";
      refresh.onclick = () => refreshAccount(a.name); td.appendChild(refresh);
    }
    const btn = document.createElement("button"); btn.className = "danger"; btn.textContent = "Remove";
    btn.onclick = () => removeAccount(a.name); td.appendChild(btn); r.appendChild(td);
  }
}

async function loadCodexAccounts() {
  const body = $("codex-accounts"); body.textContent = "";
  let data, res;
  try { res = await fetch("/admin/accounts/codex"); data = await res.json(); }
  catch (e) { const r = body.insertRow(); const c = cell(r, "Failed to load Codex accounts"); c.colSpan = 4; return; }
  if (!res.ok) { const r = body.insertRow(); const c = cell(r, (data.error && data.error.message) || "Failed to load Codex accounts"); c.colSpan = 4; return; }
  const list = (data && data.accounts) || [];
  if (!list.length) { const r = body.insertRow(); const c = cell(r, "No Codex store accounts yet"); c.colSpan = 4; c.className = "muted"; return; }
  for (const a of list) {
    const r = body.insertRow();
    cell(r, a.name); cell(r, when(a.expires_at)); cell(r, a.account_id || "—", true);
    const td = document.createElement("td");
    const btn = document.createElement("button"); btn.className = "danger"; btn.textContent = "Remove";
    btn.onclick = () => removeCodexAccount(a.name); td.appendChild(btn); r.appendChild(td);
  }
}

// Every mutation handler reloads the pool table, so two loads can be in
// flight at once. Without this counter the older response renders last and
// wins, which after a Refresh means the table falls back to the pre-probe
// `needs_relogin`. The table is also cleared *after* the await, so a
// superseded load never blanks the rows the newer one drew.
let poolLoadSeq = 0;
async function loadPool() {
  const seq = ++poolLoadSeq;
  let data, res, failure = null;
  try { res = await fetch("/admin/pool"); data = await res.json(); }
  catch (e) { failure = "Failed to load pool"; }
  if (seq !== poolLoadSeq) return;
  const body = $("pool"); body.textContent = "";
  if (failure || !res.ok) { const r = body.insertRow(); const c = cell(r, failure || (data && data.error && data.error.message) || "Failed to load pool"); c.colSpan = 9; return; }
  const providers = (data && data.providers) || [];
  let rows = 0;
  for (const p of providers) for (const a of (p.accounts || [])) {
    rows++; const r = body.insertRow();
    cell(r, p.provider); cell(r, a.name); cell(r, titleCase(a.plan) || "—");
    // `needs_relogin` is checked before the cooldown states on purpose: a dead
    // credential is *also* cooling down, and reporting only "cooling" is what
    // made a permanently dead account indistinguishable from a quota pause.
    const state = a.disabled ? "disabled" : a.needs_relogin ? "needs re-login" : !a.has_state ? "unseen" : a.near_quota ? "near quota" : a.cooldown_secs_remaining ? "cooling" : a.cooldown_fable_secs_remaining ? "cooling (fable)" : "available";
    const stateCell = cell(r, state);
    if (a.needs_relogin) {
      stateCell.className = "status"; stateCell.dataset.state = "needs-relogin";
      stateCell.title = "The stored credential was permanently rejected, or cannot be refreshed. Re-add this account to sign in again.";
    }
    const c5 = cell(r, pctReset(a.utilization_5h, a.reset_5h));
    if (a.reset_5h) c5.title = "resets " + new Date(a.reset_5h * 1000).toLocaleString();
    const c7 = cell(r, pctReset(a.utilization_7d, a.reset_7d));
    if (a.reset_7d) c7.title = "resets " + new Date(a.reset_7d * 1000).toLocaleString();
    const c7oi = cell(r, pctReset(a.utilization_7d_oi, a.reset_7d_oi));
    if (a.reset_7d_oi) c7oi.title = "resets " + new Date(a.reset_7d_oi * 1000).toLocaleString();
    cell(r, a.status || "—");
    cell(r, [a.cooldown_secs_remaining ? a.cooldown_secs_remaining + "s" : null, a.cooldown_fable_secs_remaining ? a.cooldown_fable_secs_remaining + "s (fable)" : null].filter(Boolean).join(" · ") || "—");
  }
  if (!rows) { const r = body.insertRow(); const c = cell(r, "No pooled accounts configured"); c.colSpan = 9; c.className = "muted"; }
}

function statusLabel(indicator) {
  return ({ none: "Operational", minor: "Minor issues", major: "Major outage", critical: "Critical outage", unknown: "Unknown" })[indicator] || indicator;
}

// `[server.status]` is opt-in and observation-only. Zero configured sources
// means the feature is off; configured-but-unpolled sources arrive as Unknown.
async function loadStatus() {
  const section = $("status-section");
  let data, res;
  try { res = await fetch("/admin/status"); data = await res.json(); }
  catch (e) { section.style.display = "none"; return; }
  if (!res.ok) { section.style.display = "none"; return; }
  const sources = (data && data.sources) || [];
  if (!sources.length) { section.style.display = "none"; return; }
  section.style.display = "";
  const body = $("status"); body.textContent = "";
  for (const s of sources) {
    const r = body.insertRow();
    cell(r, s.provider);
    const status = cell(r, statusLabel(s.indicator)); status.className = "status"; status.dataset.state = s.indicator;
    const detail = cell(r, s.error || s.description || "—");
    // The visible text is already short (the server keeps error details to a
    // single line, ~160 chars); the title attribute just makes that same
    // detail reachable on hover too, same as the other tables' use of title
    // for out-of-band context.
    if (s.error) detail.title = s.error;
    cell(r, when(s.observed_at ? s.observed_at * 1000 : null));
  }
}

function showMsg(id, text, ok) { const el = $(id); el.className = "msg " + (ok ? "ok" : "err"); el.textContent = text; }

function selectedMode() {
  const selected = document.querySelector('input[name="mode"]:checked');
  return selected ? selected.value : "oauth";
}
function updateModeHelp() {
  $("modehelp").textContent = selectedMode() === "setup_token"
    ? "Setup token creates a one-year, inference-only login that cannot refresh."
    : "Full OAuth creates a refreshable login that shunt manages.";
}
for (const input of document.querySelectorAll('input[name="mode"]')) { input.onchange = updateModeHelp; }

let currentName = null;
$("start").onclick = async () => {
  const name = $("name").value.trim();
  $("addmsg").className = ""; $("addmsg").textContent = "";
  try {
    const mode = selectedMode();
    const res = await fetch("/admin/accounts/claude", { method: "POST", headers: H, body: JSON.stringify({ name, mode }) });
    const data = await res.json();
    if (!res.ok) { showMsg("addmsg", (data.error && data.error.message) || "Failed to start", false); return; }
    currentName = data.name;
    $("authlink").textContent = data.authorize_url; $("authlink").href = data.authorize_url;
    $("step2").style.display = "block";
  } catch (e) { showMsg("addmsg", "Request failed", false); }
};

$("complete").onclick = async () => {
  const code = $("code").value.trim();
  try {
    const res = await fetch("/admin/accounts/claude/" + encodeURIComponent(currentName) + "/complete",
      { method: "POST", headers: H, body: JSON.stringify({ code }) });
    const data = await res.json();
    if (!res.ok) { showMsg("addmsg", (data.error && data.error.message) || "Failed to complete", false); return; }
    showMsg("addmsg", data.message || "Account stored", true);
    $("step2").style.display = "none"; $("name").value = ""; $("code").value = "";
    loadObserved(); loadAccounts(); loadPool();
  } catch (e) { showMsg("addmsg", "Request failed", false); }
};

async function removeAccount(name) {
  if (!confirm("Remove account '" + name + "'? This deletes its stored token file.")) return;
  try {
    const res = await fetch("/admin/accounts/claude/" + encodeURIComponent(name), { method: "DELETE", headers: H });
    if (!res.ok) { const data = await res.json().catch(() => ({})); showMsg("addmsg", (data.error && data.error.message) || "Failed to remove", false); return; }
    loadObserved(); loadAccounts(); loadPool();
  } catch (e) { showMsg("addmsg", "Request failed", false); }
}

async function refreshAccount(name) {
  try {
    const res = await fetch("/admin/accounts/claude/" + encodeURIComponent(name) + "/refresh", { method: "POST", headers: H });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) { showMsg("addmsg", (data.error && data.error.message) || "Refresh failed", false); loadAccounts(); loadPool(); return; }
    showMsg("addmsg", data.message || "Refresh succeeded", true);
    loadAccounts(); loadPool();
  } catch (e) { showMsg("addmsg", "Request failed", false); }
}

let currentCodexName = null;
$("start-codex").onclick = async () => {
  const name = $("codex-name").value.trim();
  $("codex-addmsg").className = ""; $("codex-addmsg").textContent = "";
  try {
    const res = await fetch("/admin/accounts/codex", { method: "POST", headers: H, body: JSON.stringify({ name }) });
    const data = await res.json();
    if (!res.ok) { showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to start Codex login", false); return; }
    currentCodexName = data.name;
    $("codex-authlink").textContent = data.authorize_url; $("codex-authlink").href = data.authorize_url;
    $("codex-step2").style.display = "block";
  } catch (e) { showMsg("codex-addmsg", "Request failed", false); }
};

$("complete-codex").onclick = async () => {
  const code = $("codex-code").value.trim();
  try {
    const res = await fetch("/admin/accounts/codex/" + encodeURIComponent(currentCodexName) + "/complete",
      { method: "POST", headers: H, body: JSON.stringify({ code }) });
    const data = await res.json();
    if (!res.ok) { showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to complete Codex login", false); return; }
    showMsg("codex-addmsg", data.message || "Codex account stored", true);
    $("codex-step2").style.display = "none"; $("codex-name").value = ""; $("codex-code").value = "";
    loadObserved(); loadCodexAccounts(); loadPool();
  } catch (e) { showMsg("codex-addmsg", "Request failed", false); }
};

async function removeCodexAccount(name) {
  if (!confirm("Remove Codex account '" + name + "'? This deletes its stored token file.")) return;
  try {
    const res = await fetch("/admin/accounts/codex/" + encodeURIComponent(name), { method: "DELETE", headers: H });
    if (!res.ok) { const data = await res.json().catch(() => ({})); showMsg("codex-addmsg", (data.error && data.error.message) || "Failed to remove Codex account", false); return; }
    loadObserved(); loadCodexAccounts(); loadPool();
  } catch (e) { showMsg("codex-addmsg", "Request failed", false); }
}

loadObserved(); loadAccounts(); loadCodexAccounts(); loadPool(); loadStatus();"#;
