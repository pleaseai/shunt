import { describe, expect, test } from 'vitest'

import { endpointOf } from '../hooks/endpoint'
import { NO_BASE_URL_TEXT, NO_TOKEN_TEXT } from '../hooks/names'

const endpoint = (resolved: ReturnType<typeof endpointOf>) => {
  if ('problem' in resolved) {
    throw new Error(`expected an endpoint, got: ${resolved.problem}`)
  }

  return resolved.endpoint
}

describe('endpoint', () => {
  test('asks the gateway this session already talks to', () => {
    const { url } = endpoint(
      endpointOf({
        anthropicBaseUrl: 'http://127.0.0.1:3001',
        anthropicAuthToken: 'token',
      }),
    )

    expect(url).toBe('http://127.0.0.1:3001/usage')
  })

  test('SHUNT_BASE_URL overrides the session gateway', () => {
    const { base } = endpoint(
      endpointOf({
        shuntBaseUrl: 'http://elsewhere:9000',
        anthropicBaseUrl: 'http://127.0.0.1:3001',
        anthropicAuthToken: 'token',
      }),
    )

    expect(base).toBe('http://elsewhere:9000')
  })

  test('a trailing slash on the base does not double the separator', () => {
    const { url } = endpoint(
      endpointOf({
        anthropicBaseUrl: 'https://gateway.example.com///',
        anthropicAuthToken: 'token',
      }),
    )

    expect(url).toBe('https://gateway.example.com/usage')
  })

  test('with no base URL it asks nothing rather than calling Anthropic', () => {
    const resolved = endpointOf({ anthropicAuthToken: 'token' })

    expect(resolved).toEqual({ problem: NO_BASE_URL_TEXT })
  })

  test('an empty base URL counts as none', () => {
    const resolved = endpointOf({
      anthropicBaseUrl: '   ',
      anthropicAuthToken: 'token',
    })

    expect(resolved).toEqual({ problem: NO_BASE_URL_TEXT })
  })

  test('a blank SHUNT_BASE_URL falls back rather than shadowing', () => {
    const { base } = endpoint(
      endpointOf({
        shuntBaseUrl: '',
        anthropicBaseUrl: 'http://127.0.0.1:3001',
        anthropicAuthToken: 'token',
      }),
    )

    expect(base).toBe('http://127.0.0.1:3001')
  })

  test('ANTHROPIC_AUTH_TOKEN rides as a Bearer, as Claude Code sends it', () => {
    const { headers } = endpoint(
      endpointOf({
        anthropicBaseUrl: 'http://gateway',
        anthropicAuthToken: 'token',
      }),
    )

    expect(headers).toEqual({ authorization: 'Bearer token' })
  })

  test('ANTHROPIC_API_KEY rides as x-api-key, as Claude Code sends it', () => {
    const { headers } = endpoint(
      endpointOf({ anthropicBaseUrl: 'http://gateway', anthropicApiKey: 'key' }),
    )

    expect(headers).toEqual({ 'x-api-key': 'key' })
  })

  test('SHUNT_TOKEN wins over both and rides as a Bearer', () => {
    const { headers } = endpoint(
      endpointOf({
        anthropicBaseUrl: 'http://gateway',
        shuntToken: 'override',
        anthropicAuthToken: 'token',
        anthropicApiKey: 'key',
      }),
    )

    expect(headers).toEqual({ authorization: 'Bearer override' })
  })

  test('a blank SHUNT_TOKEN falls back rather than shadowing', () => {
    const { headers } = endpoint(
      endpointOf({
        anthropicBaseUrl: 'http://gateway',
        shuntToken: '   ',
        anthropicAuthToken: 'token',
      }),
    )

    expect(headers).toEqual({ authorization: 'Bearer token' })
  })

  test('with no credential it says so rather than asking unauthenticated', () => {
    const resolved = endpointOf({ anthropicBaseUrl: 'http://gateway' })

    expect(resolved).toEqual({ problem: NO_TOKEN_TEXT })
  })
})
