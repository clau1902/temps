// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { z } from 'zod'
import type { CreateProviderKeyRequest } from '@/api/client'
import { isSelfHostedProvider } from '@/lib/ai-providers'
import {
  DEFAULT_SELF_HOSTED_CAPABILITIES,
  selfHostedDiscoverySignature,
  selfHostedSubmitBlocker,
} from './self-hosted-provider'

const capabilitiesSchema = z.object({
  streaming: z.boolean(),
  tools: z.boolean(),
  embeddings: z.boolean(),
  responses_api: z.boolean(),
})

/** Models a self-hosted server listed, and the inputs they were listed for. */
const discoverySchema = z
  .object({ signature: z.string(), models: z.array(z.string()) })
  .nullable()

/**
 * The add-provider-key form. Self-hosted (OpenAI-compatible) providers need
 * a base URL and a default model picked from the server's own listing, and
 * may omit the API key; every other provider needs an API key.
 */
export const providerKeyFormSchema = z
  .object({
    provider: z.string().min(1, 'Please select a provider'),
    display_name: z.string().trim().min(1, 'Please enter a display name'),
    api_key: z.string(),
    base_url: z.string(),
    default_model: z.string(),
    trust_private_address: z.boolean(),
    capabilities: capabilitiesSchema,
    discovery: discoverySchema,
  })
  .superRefine((values, ctx) => {
    if (!isSelfHostedProvider(values.provider)) {
      if (!values.api_key.trim()) {
        ctx.addIssue({
          code: 'custom',
          path: ['api_key'],
          message: 'Please enter an API key',
        })
      }
      return
    }
    const blocker = selfHostedSubmitBlocker({
      baseUrl: values.base_url,
      discoveredModels: currentDiscoveredModels(values),
      defaultModel: values.default_model,
    })
    if (blocker) {
      ctx.addIssue({
        code: 'custom',
        path: [values.base_url.trim() ? 'default_model' : 'base_url'],
        message: blocker,
      })
    }
  })

export type ProviderKeyFormValues = z.infer<typeof providerKeyFormSchema>

export const EMPTY_PROVIDER_KEY_FORM: ProviderKeyFormValues = {
  provider: '',
  display_name: '',
  api_key: '',
  base_url: '',
  default_model: '',
  trust_private_address: false,
  capabilities: DEFAULT_SELF_HOSTED_CAPABILITIES,
  discovery: null,
}

/** The discovered models, if they were listed for the current inputs. */
export function currentDiscoveredModels(
  values: Pick<
    ProviderKeyFormValues,
    'base_url' | 'api_key' | 'trust_private_address' | 'discovery'
  >
): string[] | null {
  const signature = selfHostedDiscoverySignature({
    baseUrl: values.base_url,
    apiKey: values.api_key,
    trustPrivateAddress: values.trust_private_address,
  })
  return values.discovery?.signature === signature
    ? values.discovery.models
    : null
}

/** Request body for `POST /ai/providers`. */
export function providerKeyFormToRequest(
  values: ProviderKeyFormValues
): CreateProviderKeyRequest {
  const base = {
    provider: values.provider,
    display_name: values.display_name.trim(),
    api_key: values.api_key,
    base_url: values.base_url.trim() || undefined,
  }
  if (!isSelfHostedProvider(values.provider)) return base
  return {
    ...base,
    default_model: values.default_model,
    trust_private_address: values.trust_private_address,
    capabilities: values.capabilities,
  }
}
