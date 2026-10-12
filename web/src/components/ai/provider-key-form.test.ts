// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  EMPTY_PROVIDER_KEY_FORM,
  providerKeyFormSchema,
  providerKeyFormToRequest,
  type ProviderKeyFormValues,
} from './provider-key-form'
import { selfHostedDiscoverySignature } from './self-hosted-provider'

const MODEL = 'mlx-community/Some-Model-4bit'

function selfHosted(
  overrides: Partial<ProviderKeyFormValues> = {}
): ProviderKeyFormValues {
  const values: ProviderKeyFormValues = {
    ...EMPTY_PROVIDER_KEY_FORM,
    provider: 'openai_compatible',
    display_name: 'Local MLX server',
    base_url: 'http://10.0.0.5:8000/v1',
    trust_private_address: true,
    default_model: MODEL,
    ...overrides,
  }
  if ('discovery' in overrides) return values
  return {
    ...values,
    discovery: {
      signature: selfHostedDiscoverySignature({
        baseUrl: values.base_url,
        apiKey: values.api_key,
        trustPrivateAddress: values.trust_private_address,
      }),
      models: [MODEL],
    },
  }
}

function issues(values: ProviderKeyFormValues) {
  const result = providerKeyFormSchema.safeParse(values)
  return result.success
    ? []
    : result.error.issues.map(
        (issue) => `${issue.path.join('.')}: ${issue.message}`
      )
}

describe('provider key form', () => {
  test('vendor providers need a display name and an API key', () => {
    expect(issues({ ...EMPTY_PROVIDER_KEY_FORM, provider: 'openai' })).toEqual([
      'display_name: Please enter a display name',
      'api_key: Please enter an API key',
    ])
    expect(
      issues({
        ...EMPTY_PROVIDER_KEY_FORM,
        provider: 'openai',
        display_name: 'OpenAI',
        api_key: 'sk-x',
      })
    ).toEqual([])
  })

  test('a self-hosted provider needs a discovered default model, not an API key', () => {
    expect(issues(selfHosted())).toEqual([])
    expect(issues(selfHosted({ base_url: '' }))[0]).toContain('base_url')
    expect(issues(selfHosted({ discovery: null }))[0]).toContain(
      'Load the models'
    )
    expect(issues(selfHosted({ default_model: 'gpt-5-nano' }))[0]).toContain(
      'Pick a default model'
    )
  })

  test('a discovery made for other inputs no longer counts', () => {
    const values = selfHosted()
    expect(
      issues({ ...values, base_url: 'http://10.0.0.6:8000/v1' })[0]
    ).toContain('Load the models')
  })

  test('only self-hosted requests carry the self-hosted fields', () => {
    expect(providerKeyFormToRequest(selfHosted())).toEqual({
      provider: 'openai_compatible',
      display_name: 'Local MLX server',
      api_key: '',
      base_url: 'http://10.0.0.5:8000/v1',
      default_model: MODEL,
      trust_private_address: true,
      capabilities: EMPTY_PROVIDER_KEY_FORM.capabilities,
    })
    expect(
      providerKeyFormToRequest({
        ...EMPTY_PROVIDER_KEY_FORM,
        provider: 'openai',
        display_name: ' OpenAI ',
        api_key: 'sk-x',
        trust_private_address: true,
      })
    ).toEqual({
      provider: 'openai',
      display_name: 'OpenAI',
      api_key: 'sk-x',
      base_url: undefined,
    })
  })
})
