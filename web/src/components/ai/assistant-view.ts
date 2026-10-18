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

/** What a keydown's target needs to be asked: is this a place you type? */
interface ShortcutTarget {
  tagName?: string
  isContentEditable?: boolean
  closest?: (selector: string) => unknown
}

/** Whether keystrokes aimed at `target` are text being typed, or a dialog's. */
function swallowsTyping(target: unknown): boolean {
  if (!target || typeof target !== 'object') return false
  const element = target as ShortcutTarget
  const tag = element.tagName?.toUpperCase()
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true
  if (element.isContentEditable) return true
  // Focus inside a dialog or menu belongs to it; "/" must not pull it away.
  return Boolean(element.closest?.('[role="dialog"], [role="menu"]'))
}

/**
 * Whether a keydown is a bare "/" that should focus the message box: no
 * ⌘/Ctrl/Alt, not part of an IME composition, not already handled, and not
 * aimed at somewhere you are typing. Shift is allowed because "/" needs it on
 * many keyboard layouts.
 */
export function isFocusComposerShortcut(
  event: Pick<
    KeyboardEvent,
    | 'key'
    | 'metaKey'
    | 'ctrlKey'
    | 'altKey'
    | 'isComposing'
    | 'defaultPrevented'
  > & { target: EventTarget | null }
): boolean {
  return (
    event.key === '/' &&
    !event.metaKey &&
    !event.ctrlKey &&
    !event.altKey &&
    !event.isComposing &&
    !event.defaultPrevented &&
    !swallowsTyping(event.target)
  )
}

/** Appended to a composer's placeholder so the shortcut can be found. */
export const FOCUS_COMPOSER_HINT = ' (press / to focus)'
