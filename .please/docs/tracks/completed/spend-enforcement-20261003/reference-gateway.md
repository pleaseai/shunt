# Reference gateway behavior (spend limits)

Source: the Claude apps gateway bundled in the Claude Code 2.1.288 native binary
(`strings` of `~/.local/share/claude/versions/2.1.288`), including the
`GET /protocol` text it serves ("Usage-limit headers — optional"), cross-checked
against <https://code.claude.com/docs/en/claude-apps-gateway-spend-limits>.
Extracted 2026-10-04. This note resolves the STOP conditions on T006 (header
names/formats) and T007 (`SpendSummary` field names).

## Refusal message

```
base = cap ? `spend limit reached (${period}; resets ${iso.slice(0,16).replace("T"," ")} UTC)`
           : "spend limit unavailable"
message = blocked_message ? `${base} — ${blocked_message}` : base   // U+2014 with spaces
```

Status 429, `error.type` `billing_error`, `x-should-retry: false`, plus the
headers below. The fail-closed refusal gets the same suffix rule.

## Binding cap (which cap the headers describe)

Per period with a non-null cap: `exceeded = spent >= cap`,
`utilization = cap > 0 ? spent / cap : 1`, `resetsAt` = next UTC day / Monday /
1st. Fold over periods (daily, weekly, monthly order) with:

- one exceeded, the other not → the exceeded one;
- both exceeded → the one that resets later;
- neither → the higher utilization (a tie keeps the later period).

A principal with no non-null cap gets no unified headers at all.

## Header values (`Rue`)

```
reset      = floor(resetsAt / 1000)                       // Unix seconds
threshold  = exceeded ? 1 : first of [0.95, 0.75] with utilization > t
status     = exceeded ? "rejected" : threshold ? "allowed_warning" : "allowed"
util_out   = round(utilization * 100) / 100, capped at 0.99 unless exceeded

anthropic-ratelimit-unified-status:              status
anthropic-ratelimit-unified-reset:               reset
anthropic-ratelimit-unified-overage-reset:       reset
anthropic-ratelimit-unified-overage-utilization: String(util_out)   // JS number formatting: "0.82", "0.5", "0", "1", "1.2"
anthropic-ratelimit-unified-overage-surpassed-threshold: String(threshold)   // only when threshold set: "0.95" | "0.75" | "1"
if exceeded:
  anthropic-ratelimit-unified-overage-period:          daily|weekly|monthly
  anthropic-ratelimit-unified-overage-disabled-reason: org_spend_cap_reached
  retry-after: max(1, ceil((resetsAt - now) / 1000))
else:
  anthropic-ratelimit-unified-representative-claim: overage
  anthropic-ratelimit-unified-overage-status:       status
```

- Success (2xx) responses for a capped principal carry the non-exceeded set.
  Upstream `anthropic-ratelimit-*` response headers are stripped first.
- The over-cap 429 carries the exceeded set. It omits `representative-claim`
  and `overage-status`, because with them the client composes its own line and
  drops `error.message`.
- Fail-open (store unreadable, default) means the request proceeds with no
  unified headers.
- Fail-closed means a 429 with `x-should-retry: false`,
  `anthropic-ratelimit-unified-overage-disabled-reason: fetch_error`, and the
  message, nothing else.

## `GET /v1/organizations/spend_limits/effective`

Response: `{"data": [row...], "next_page": token|null}`.

```json
{
  "scope": {"type": "user", "user_id": "<principal>"},
  "groups": [],
  "actor": {"type": "user_actor", "user_id": "<principal>", "name": null, "email_address": null, "deleted": false},
  "amount": "50000" | null,
  "currency": "USD",
  "period": "daily" | "weekly" | "monthly",
  "source": {"type": "user", "user_id": "..."} | {"type": "organization"} | null,
  "spend_limit_id": "spl_..." | null,
  "period_to_date_spend": "123.456"
}
```

- `period_to_date_spend` is cents in the current window, with up to 3 decimals
  and trailing zeros stripped (`toFixed(3).replace(/\.?0+$/,"")`). This gives
  `"0"`, `"12.5"` and `"1234"`.
- `amount` / `source` / `spend_limit_id` describe the effective cap; all three
  are null when no cap applies.
- There is one row per principal × requested period. Periods follow the
  deduplicated `period[]` order, and default to daily, weekly, monthly. A row
  is emitted even when the principal has no cap or no spend in that window.

Validation is 400 in the admin error envelope, checked in this order:

| Parameter | Rule | Error message |
| - | - | - |
| `limit` | 1–1000, default 20 | `limit: must be between 1 and 1000` |
| `period[]` | each value is `daily`, `weekly` or `monthly` | `period[]: must be one of daily, weekly, monthly` |
| `user_ids[]` | at most 100 entries | `user_ids[]: at most 100 entries per request` |
| `q` | at most 256 characters | `q: too long` |
| `sort` | `spend_desc` only | `sort: must be spend_desc` |
| `sort` | `spend_desc` requires exactly one `period[]` | `sort=spend_desc requires exactly one period[]` |
| `page` | must decode | `page: invalid page token` |

How the rows are selected:

- **With `user_ids[]`:** rows for exactly those principals, in the given order. There is no pagination and `next_page` is null.
- **Without it:** principals with any recorded spend.
- **Default order:** principal ascending. The page token is a cursor on the last principal.
- **`sort=spend_desc`:** current-window spend for the single period, descending, with ties broken by principal ascending. The cursor holds `(principal, cents)`.
- **`q`:** a case-insensitive substring match over the principal id and the last-seen email and display name.
- **Page token:** base64url JSON `{"p": "alice"}` or `{"p": "alice", "c": 123, "s": true}` (`c` is the cursor's spend in cents). It is opaque, so ours may differ.
- **Fetching:** fetch `limit + 1` rows; `next_page` is set only when more remain.

Out of scope here: the gateway also answers `GET /api/oauth/usage` (the
dollars in `/usage`). The spec defers this.
