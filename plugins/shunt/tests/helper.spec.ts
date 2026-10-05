import { describe, expect, test } from 'vitest'

import { shuntHelperArgvOf } from '../hooks/helper'

describe('shunt apiKeyHelper', () => {
  test.each([
    ["'/usr/local/bin/shunt' gateway token", ['/usr/local/bin/shunt', 'gateway', 'token']],
    ["'/Applications/My Tools/shunt' gateway token", ['/Applications/My Tools/shunt', 'gateway', 'token']],
    ["'/opt/it'\\''s/shunt' gateway token", ["/opt/it's/shunt", 'gateway', 'token']],
    ['"C:\\Program Files\\shunt\\shunt.exe" gateway token', ['C:\\Program Files\\shunt\\shunt.exe', 'gateway', 'token']],
    ['shunt gateway token', ['shunt', 'gateway', 'token']],
    ['  ~/bin/shunt   gateway  token  ', ['~/bin/shunt', 'gateway', 'token']],
  ])('runs %j as its argv', (command, argv) => {
    expect(shuntHelperArgvOf(command)).toEqual(argv)
  })

  test.each([
    [undefined],
    [''],
    ['op read op://vault/anthropic/key'],
    ['shunt gateway token; rm -rf ~'],
    ['shunt gateway login'],
    ['/usr/bin/not-shunt gateway token'],
    ["'/usr/bin/shunt' gateway token extra"],
  ])('leaves %j alone', command => {
    expect(shuntHelperArgvOf(command)).toBeNull()
  })
})
