// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Which layout the full-screen assistant uses.
 * - `chats`: the chat list beside the conversation.
 * - `work`: the conversation beside its working tabs (deployments), with the
 *   chat list tucked into a switcher in the page header.
 */
export type AssistantView = 'chats' | 'work'

export const ASSISTANT_VIEW_STORAGE_KEY = 'temps.assistant.view'

export function parseAssistantView(raw: string | null): AssistantView {
  return raw === 'work' ? 'work' : 'chats'
}

type ViewStorage = Pick<Storage, 'getItem' | 'setItem'>

function defaultStorage(): ViewStorage | null {
  try {
    return typeof window === 'undefined' ? null : window.localStorage
  } catch {
    // Storage can be blocked (private mode); the view just isn't remembered.
    return null
  }
}

export function readAssistantView(
  storage: ViewStorage | null = defaultStorage()
): AssistantView {
  try {
    return parseAssistantView(
      storage?.getItem(ASSISTANT_VIEW_STORAGE_KEY) ?? null
    )
  } catch {
    return 'chats'
  }
}

export function writeAssistantView(
  view: AssistantView,
  storage: ViewStorage | null = defaultStorage()
): void {
  try {
    storage?.setItem(ASSISTANT_VIEW_STORAGE_KEY, view)
  } catch {
    // Not remembered; the view still applies for this visit.
  }
}

/** Whether a keydown is the ⌘K / Ctrl+K shortcut. */
export function isSwitcherShortcut(
  event: Pick<
    KeyboardEvent,
    'key' | 'metaKey' | 'ctrlKey' | 'altKey' | 'shiftKey'
  >
): boolean {
  return (
    event.key.toLowerCase() === 'k' &&
    (event.metaKey || event.ctrlKey) &&
    !event.altKey &&
    !event.shiftKey
  )
}
