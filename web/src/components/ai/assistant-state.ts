// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { isInternalConsolePath } from '@/lib/cloud-telemetry-activation'
import type { ChatDetailLevel } from './chat-detail-level'

/** `context_type` of conversations started from the Temps assistant. */
export const ASSISTANT_CONTEXT_TYPE = 'assistant'

/**
 * Where an operator pastes the one key the assistant needs. The query opens the
 * add-key dialog on the AI Gateway page with OpenRouter preselected.
 */
export const OPENROUTER_SETUP_PATH = '/ai-gateway?add=openrouter'

export const NO_PROVIDER_REASON =
  'No AI provider is configured. Add an OpenRouter API key to start managing Temps by chat.'

/** Concrete things to ask, shown before the first message. */
export const ASSISTANT_EXAMPLE_PROMPTS: readonly string[] = [
  'Which of my deployments failed this week, and why?',
  'Show me the environment variables set on my newest project',
  'Create a staging environment for my API project',
]

export function isAssistantConversation(conversation: {
  context_type: string
}): boolean {
  return conversation.context_type === ASSISTANT_CONTEXT_TYPE
}

/** The fields of the provider-status capability response the assistant reads. */
export interface AssistantCapabilityInput {
  gateway_available: boolean
  reason?: string | null
  setup_path?: string | null
}

export type AssistantReadiness =
  | { state: 'ready' }
  | { state: 'unconfigured'; reason: string; setupPath: string }

/**
 * Turn the capability response into what the UI renders. The assistant runs on
 * an AI gateway provider, so a host harness being ready does not count.
 *
 * Always yields a reason and a console path when unconfigured: a server that
 * omits them, or sends an off-console `setup_path`, must not leave the user
 * with a bare disabled control.
 */
export function assistantReadiness(
  status: AssistantCapabilityInput | undefined
): AssistantReadiness {
  if (status?.gateway_available) return { state: 'ready' }
  return { state: 'unconfigured', ...unconfiguredAssistant(status) }
}

/** The reason and fix to show when there is no usable provider. */
export function unconfiguredAssistant(
  status: Partial<AssistantCapabilityInput> | undefined
): { reason: string; setupPath: string } {
  return {
    reason: status?.reason?.trim() || NO_PROVIDER_REASON,
    setupPath: isInternalConsolePath(status?.setup_path)
      ? status.setup_path
      : OPENROUTER_SETUP_PATH,
  }
}

/** Pull the server's `setup_path` out of a 409 problem document, if it sent one. */
export function setupPathFromProblem(problem: unknown): string | undefined {
  if (typeof problem !== 'object' || problem === null) return undefined
  const path = (problem as { setup_path?: unknown }).setup_path
  return isInternalConsolePath(typeof path === 'string' ? path : undefined)
    ? (path as string)
    : undefined
}

const ADVANCED_KEY = 'temps.ai.assistant.advanced'

export function loadDetailLevel(
  storage: Pick<Storage, 'getItem'> | undefined = safeStorage()
): ChatDetailLevel {
  try {
    return storage?.getItem(ADVANCED_KEY) === '1' ? 'advanced' : 'simple'
  } catch {
    return 'simple'
  }
}

export function saveDetailLevel(
  level: ChatDetailLevel,
  storage: Pick<Storage, 'setItem' | 'removeItem'> | undefined = safeStorage()
): void {
  try {
    if (level === 'advanced') storage?.setItem(ADVANCED_KEY, '1')
    else storage?.removeItem(ADVANCED_KEY)
  } catch {
    /* storage unavailable — the choice just won't persist */
  }
}

function safeStorage(): Storage | undefined {
  try {
    return typeof localStorage === 'undefined' ? undefined : localStorage
  } catch {
    return undefined
  }
}
