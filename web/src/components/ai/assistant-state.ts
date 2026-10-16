// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { isInternalConsolePath } from '@/lib/cloud-telemetry-activation'

/** `context_type` of conversations started from the Temps assistant. */
export const ASSISTANT_CONTEXT_TYPE = 'assistant'

/**
 * Where an operator pastes the one key the assistant needs. The query opens the
 * add-key dialog on the AI Gateway page with OpenRouter preselected.
 */
export const OPENROUTER_SETUP_PATH = '/ai-gateway?add=openrouter'

export const NO_PROVIDER_REASON =
  'No AI provider is configured. Add an OpenRouter API key to start managing Temps by chat.'

/** Gateway permission mode that has every change wait for the user's approval. */
export const ASK_BEFORE_CHANGES_MODE = 'confirm-actions'

/**
 * Gateway permission mode that applies changes without asking. Deletions still
 * wait for approval: the server never auto-approves them.
 */
export const RUN_CHANGES_AUTOMATICALLY_MODE = 'auto'

/** Whether a conversation's permission mode makes changes wait for approval. */
export function asksBeforeChanges(permissionModeId: string | null): boolean {
  return permissionModeId === ASK_BEFORE_CHANGES_MODE
}

export function permissionModeForAsk(ask: boolean): string {
  return ask ? ASK_BEFORE_CHANGES_MODE : RUN_CHANGES_AUTOMATICALLY_MODE
}

/**
 * Outcomes people open the assistant for, shown before the first message. Each
 * names a goal, not a question: the assistant works out the steps itself.
 */
export const ASSISTANT_EXAMPLE_PROMPTS: readonly string[] = [
  'Deploy my app from a Git repository URL',
  'Set up daily backups of my databases to S3',
  'Add a Postgres database to one of my projects',
  'Why did my last deployment fail?',
]

const LAST_MODEL_KEY = 'temps.assistant.last-model'

/** The model the user last started or switched a chat to, if any. */
export function loadLastAssistantModel(): string | null {
  try {
    return window.localStorage.getItem(LAST_MODEL_KEY)
  } catch {
    return null
  }
}

/**
 * Remember a model so the next new chat starts on it. This is only a starting
 * point for new chats: every chat keeps the model it was given.
 */
export function saveLastAssistantModel(modelId: string): void {
  try {
    window.localStorage.setItem(LAST_MODEL_KEY, modelId)
  } catch {
    // Storage can be unavailable (private mode); the default still applies.
  }
}

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
