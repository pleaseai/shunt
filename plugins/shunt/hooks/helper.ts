/**
 * Recognizes shunt's own `apiKeyHelper`, so the mod can ask it for the
 * gateway login token a `shunt gateway claude` session authenticates with.
 *
 * That launcher scrubs `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` from the
 * environment and hands Claude Code an inline `--settings` document whose
 * `apiKeyHelper` is `'<path>/shunt' gateway token`; Claude Code consumes the
 * helper's output itself and never re-exports it. `shunt gateway login`
 * suggests the bare `shunt gateway token` for a hand-written setting.
 *
 * Only that command is run. Any other helper — a password manager, a script
 * that prompts — is left alone: the mod would otherwise run it every few
 * minutes for a usage figure, with whatever side effects it has.
 */

/** The executable's quoted or bare spelling, then `gateway token`. */
const SHUNT_HELPER =
  /^\s*('(?:[^']|'\\'')*'|"[^"]*"|[^\s'"]+)\s+gateway\s+token\s*$/

const BASENAME = /(?:^|[\\/])shunt(?:\.exe)?$/

/**
 * Undoes the quoting `shunt gateway claude` applies: POSIX single quotes with
 * `'\''` for an embedded quote, or plain double quotes on Windows.
 */
const unquote = (word: string): string =>
  word.startsWith("'")
    ? word.slice(1, -1).replaceAll(`'\\''`, `'`)
    : word.startsWith('"')
      ? word.slice(1, -1)
      : word

/**
 * The argv to run for an `apiKeyHelper` setting when it is shunt's own, or
 * `null` for anything else (including no setting at all).
 *
 * The argv is run directly rather than through a shell, so nothing in the
 * setting is interpreted beyond the quoting the launcher writes.
 *
 * @param command the merged settings' `apiKeyHelper`, as read
 */
export function shuntHelperArgvOf(command: unknown): string[] | null {
  if (typeof command !== 'string') {
    return null
  }

  const match = SHUNT_HELPER.exec(command)
  const executable = match?.[1] === undefined ? '' : unquote(match[1])

  return BASENAME.test(executable) ? [executable, 'gateway', 'token'] : null
}
