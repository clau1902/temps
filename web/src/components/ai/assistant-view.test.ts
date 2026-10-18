// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  ASSISTANT_VIEW_STORAGE_KEY,
  isFocusComposerShortcut,
  isSwitcherShortcut,
  parseAssistantView,
  readAssistantView,
  writeAssistantView,
} from './assistant-view'

function memory(initial: Record<string, string> = {}) {
  const data = new Map(Object.entries(initial))
  return {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => void data.set(key, value),
  }
}

describe('assistant view', () => {
  test('anything but "work" is the chats view', () => {
    expect(parseAssistantView('work')).toBe('work')
    expect(parseAssistantView('chats')).toBe('chats')
    expect(parseAssistantView('sidebar')).toBe('chats')
    expect(parseAssistantView(null)).toBe('chats')
  })

  test('the choice survives a reload', () => {
    const storage = memory()
    expect(readAssistantView(storage)).toBe('chats')
    writeAssistantView('work', storage)
    expect(storage.getItem(ASSISTANT_VIEW_STORAGE_KEY)).toBe('work')
    expect(readAssistantView(storage)).toBe('work')
  })

  test('blocked storage falls back to chats without throwing', () => {
    const blocked = {
      getItem: () => {
        throw new Error('denied')
      },
      setItem: () => {
        throw new Error('denied')
      },
    }
    expect(readAssistantView(blocked)).toBe('chats')
    expect(() => writeAssistantView('work', blocked)).not.toThrow()
    expect(readAssistantView(null)).toBe('chats')
  })
})

describe('isSwitcherShortcut', () => {
  const press = (over: Partial<KeyboardEvent>) => ({
    key: 'k',
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    ...over,
  })
  test('is Cmd+K or Ctrl+K, nothing more', () => {
    expect(isSwitcherShortcut(press({ metaKey: true }))).toBe(true)
    expect(isSwitcherShortcut(press({ ctrlKey: true, key: 'K' }))).toBe(true)
    expect(isSwitcherShortcut(press({}))).toBe(false)
    expect(isSwitcherShortcut(press({ metaKey: true, shiftKey: true }))).toBe(
      false
    )
    expect(isSwitcherShortcut(press({ metaKey: true, key: 'j' }))).toBe(false)
  })
})

describe('isFocusComposerShortcut', () => {
  const press = (over: Record<string, unknown> = {}) => ({
    key: '/',
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    isComposing: false,
    defaultPrevented: false,
    target: { tagName: 'BODY' } as unknown as EventTarget,
    ...over,
  })
  const at = (target: Record<string, unknown>) =>
    press({ target: target as unknown as EventTarget })

  test('is a bare slash on the page', () => {
    expect(isFocusComposerShortcut(press())).toBe(true)
    expect(isFocusComposerShortcut(press({ target: null }))).toBe(true)
  })

  test('is not a slash typed into a field', () => {
    expect(isFocusComposerShortcut(at({ tagName: 'TEXTAREA' }))).toBe(false)
    expect(isFocusComposerShortcut(at({ tagName: 'input' }))).toBe(false)
    expect(isFocusComposerShortcut(at({ tagName: 'SELECT' }))).toBe(false)
    expect(
      isFocusComposerShortcut(at({ tagName: 'DIV', isContentEditable: true }))
    ).toBe(false)
  })

  test('leaves focus alone inside a dialog or menu', () => {
    expect(
      isFocusComposerShortcut(at({ tagName: 'BUTTON', closest: () => ({}) }))
    ).toBe(false)
    expect(
      isFocusComposerShortcut(at({ tagName: 'BUTTON', closest: () => null }))
    ).toBe(true)
  })

  test('ignores chords, compositions and handled keys', () => {
    expect(isFocusComposerShortcut(press({ metaKey: true }))).toBe(false)
    expect(isFocusComposerShortcut(press({ ctrlKey: true }))).toBe(false)
    expect(isFocusComposerShortcut(press({ altKey: true }))).toBe(false)
    expect(isFocusComposerShortcut(press({ isComposing: true }))).toBe(false)
    expect(isFocusComposerShortcut(press({ defaultPrevented: true }))).toBe(
      false
    )
    expect(isFocusComposerShortcut(press({ key: 'k' }))).toBe(false)
  })
})
