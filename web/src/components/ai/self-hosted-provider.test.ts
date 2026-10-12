// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  DEFAULT_SELF_HOSTED_CAPABILITIES,
  hasIpLiteralHost,
  normalizeSelfHostedCapabilities,
  withSavingKey,
  withoutSavingKey,
  isPlainHttpUrl,
  selfHostedDiscoverySignature,
  selfHostedSubmitBlocker,
} from './self-hosted-provider'

describe('self-hosted provider form helpers', () => {
  test('discovery is invalidated when base URL, key or trust change', () => {
    const base = {
      baseUrl: 'http://10.0.0.5:8000/v1',
      apiKey: '',
      trustPrivateAddress: false,
    }
    const signature = selfHostedDiscoverySignature(base)
    expect(
      selfHostedDiscoverySignature({
        ...base,
        baseUrl: ' http://10.0.0.5:8000/v1 ',
      })
    ).toBe(signature)
    expect(
      selfHostedDiscoverySignature({
        ...base,
        baseUrl: 'http://10.0.0.6:8000/v1',
      })
    ).not.toBe(signature)
    expect(selfHostedDiscoverySignature({ ...base, apiKey: 'k' })).not.toBe(
      signature
    )
    expect(
      selfHostedDiscoverySignature({ ...base, trustPrivateAddress: true })
    ).not.toBe(signature)
  })

  test('recognises plain http and IP-literal hosts', () => {
    expect(isPlainHttpUrl('HTTP://10.0.0.5/v1')).toBe(true)
    expect(isPlainHttpUrl('https://10.0.0.5/v1')).toBe(false)
    expect(hasIpLiteralHost('http://10.0.0.5:8000/v1')).toBe(true)
    expect(hasIpLiteralHost('http://[fd12::5]:8000/v1')).toBe(true)
    expect(hasIpLiteralHost('http://models.internal:8000/v1')).toBe(false)
    expect(hasIpLiteralHost('not a url')).toBe(false)
  })

  test('the form needs a discovered default model before it can be saved', () => {
    const models = ['mlx-community/Some-Model-4bit']
    expect(
      selfHostedSubmitBlocker({
        baseUrl: '',
        discoveredModels: null,
        defaultModel: '',
      })
    ).toContain('base URL')
    expect(
      selfHostedSubmitBlocker({
        baseUrl: 'http://10.0.0.5/v1',
        discoveredModels: null,
        defaultModel: '',
      })
    ).toContain('Load the models')
    expect(
      selfHostedSubmitBlocker({
        baseUrl: 'http://10.0.0.5/v1',
        discoveredModels: [],
        defaultModel: '',
      })
    ).toContain('no models')
    expect(
      selfHostedSubmitBlocker({
        baseUrl: 'http://10.0.0.5/v1',
        discoveredModels: models,
        defaultModel: 'gpt-5-nano',
      })
    ).toContain('Pick a default model')
    expect(
      selfHostedSubmitBlocker({
        baseUrl: 'http://10.0.0.5/v1',
        discoveredModels: models,
        defaultModel: models[0],
      })
    ).toBeNull()
  })

  test('stored capability declarations fall back to server defaults', () => {
    expect(normalizeSelfHostedCapabilities(null)).toEqual(
      DEFAULT_SELF_HOSTED_CAPABILITIES
    )
    expect(
      normalizeSelfHostedCapabilities({ embeddings: true, streaming: null })
    ).toEqual({
      ...DEFAULT_SELF_HOSTED_CAPABILITIES,
      embeddings: true,
    })
  })

  test('saving state is tracked per key, so one key finishing never frees another', () => {
    let saving: ReadonlySet<number> = new Set()
    saving = withSavingKey(saving, 1)
    saving = withSavingKey(saving, 2)
    saving = withoutSavingKey(saving, 2)
    expect(saving.has(1)).toBe(true)
    expect(saving.has(2)).toBe(false)
    expect(withoutSavingKey(saving, 1).size).toBe(0)
  })
})
