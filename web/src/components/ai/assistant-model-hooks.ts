// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { updateProviderKey } from '@/api/client'
import {
  getAiProviderStatusOptions,
  getAiProviderStatusQueryKey,
  getProviderKeyOptions,
  getProviderKeyQueryKey,
  listProviderKeysOptions,
  listProviderKeysQueryKey,
} from '@/api/client/@tanstack/react-query.gen'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useCallback, useMemo, useState } from 'react'
import { assistantModelOptions } from './assistant-models'
import {
  loadLastAssistantModel,
  saveLastAssistantModel,
} from './assistant-state'

/** The gateway key the assistant runs on: the active OpenRouter key if there is one. */
export function useAssistantProviderKey() {
  const keys = useQuery({
    ...listProviderKeysOptions(),
    staleTime: 15_000,
    retry: false,
  })
  const active = (keys.data ?? []).filter((key) => key.is_active)
  const key = active.find((item) => item.provider === 'openrouter') ?? active[0]
  return { key: key ?? null, isPending: keys.isPending }
}

/** The key's models with what the provider's catalogue says about each. */
export function useProviderModels(providerKeyId: number | null) {
  const detail = useQuery({
    ...getProviderKeyOptions({ path: { id: providerKeyId ?? 0 } }),
    enabled: providerKeyId !== null,
    staleTime: 60_000,
    retry: false,
  })
  const options = useMemo(
    () => assistantModelOptions(detail.data?.models),
    [detail.data?.models]
  )
  return { ...detail, options }
}

/** Persist the model new assistant chats start on for this key. */
export function useSetDefaultModel(providerKeyId: number | null) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async (modelId: string) => {
      if (providerKeyId === null) throw new Error('No AI provider key')
      return updateProviderKey({
        path: { id: providerKeyId },
        body: { default_model: modelId },
        throwOnError: true,
      })
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: getProviderKeyQueryKey({
            path: { id: providerKeyId ?? 0 },
          }),
        }),
        queryClient.invalidateQueries({ queryKey: listProviderKeysQueryKey() }),
        queryClient.invalidateQueries({
          queryKey: getAiProviderStatusQueryKey(),
        }),
      ])
    },
  })
}

/**
 * The model a new chat starts on when none is picked: the key's pinned default,
 * else whatever the server will actually resolve (the provider status reports
 * it per key). Never a placeholder; null only while that is still loading.
 */
export function useEffectiveDefaultModel(
  key: { id: number; default_model?: string | null } | null
): string | null {
  const status = useQuery({
    ...getAiProviderStatusOptions(),
    staleTime: 15_000,
    retry: false,
    enabled: key !== null,
  })
  if (key === null) return null
  if (key.default_model) return key.default_model
  const provider = status.data?.available_providers?.find(
    (candidate) => candidate.id === `gateway_key:${key.id}`
  )
  return provider?.default_model_id ?? null
}

/**
 * The model a new chat will be created with. Chosen per chat: picking one
 * changes only this chat. It starts on the model last used when that is still
 * offered, else the key's default, so the user is not asked again every time.
 */
export function useNewChatModel() {
  const { key } = useAssistantProviderKey()
  const models = useProviderModels(key?.id ?? null)
  const defaultModel = useEffectiveDefaultModel(key)
  const [picked, setPicked] = useState<string | null>(null)
  const [last] = useState(loadLastAssistantModel)

  const offered = (modelId: string | null) =>
    modelId !== null && models.options.some((option) => option.id === modelId)
  const model = picked ?? (offered(last) ? last : defaultModel)

  const choose = useCallback((modelId: string) => {
    setPicked(modelId)
    saveLastAssistantModel(modelId)
  }, [])
  return { model, choose }
}
