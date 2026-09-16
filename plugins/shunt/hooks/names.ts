/**
 * The command the mod answers, and the fixed texts it prints.
 *
 * The name is namespaced by the plugin (`shunt` + `commands/usage.md`), which
 * is what keeps it off the built-in `/usage`: `$.command.register` takes a
 * bare global name and refuses `usage` outright ("it is the built-in /usage"),
 * while a plugin's markdown command lists as `shunt:usage` and is free.
 */
export const COMMAND_NAME = 'shunt:usage'

/**
 * The path `[server.usage]` registers on the gateway.
 */
export const USAGE_PATH = '/usage'

export const NO_BASE_URL_TEXT =
  'no gateway to ask — neither SHUNT_BASE_URL nor ANTHROPIC_BASE_URL is ' +
  'set, so this session is talking to Anthropic directly. Point Claude Code at ' +
  'your gateway (export ANTHROPIC_BASE_URL=http://127.0.0.1:3001) and run ' +
  '/shunt:usage again.'

export const NO_TOKEN_TEXT =
  'no client token to authenticate with — GET /usage requires ' +
  '[server.auth], and none of SHUNT_TOKEN, ANTHROPIC_AUTH_TOKEN or ' +
  'ANTHROPIC_API_KEY is set. Ask the gateway operator for a client token.'

export const NOT_ENABLED_TEXT =
  'the gateway answered, but GET /usage is not enabled on it. Add an ' +
  '[server.usage] table to the gateway config and restart it; the table takes ' +
  'no keys, but it requires [server.auth] to be set as well.'

export const REFUSED_TEXT =
  'the gateway refused the client token this session is using. The first ' +
  'of SHUNT_TOKEN, ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY that is set ' +
  'is the one sent, and the rest are ignored — check that one against the ' +
  "gateway's [server.auth]."

export const HOOK_FAILED_TEXT =
  'could not read the pool usage — the hook that answers this command ' +
  'failed or ran past its budget. Run /shunt:usage again; if it keeps ' +
  'failing, check that the gateway is up and reachable.'

export const NO_POOL_TEXT =
  'the gateway reports no pooled provider, so there is no shared quota ' +
  'to show. Pool usage appears once a provider is configured with pooled ' +
  'accounts (auth mode oauth or chatgpt).'
