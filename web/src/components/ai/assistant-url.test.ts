// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  readViewParam,
  syncedPageParams,
  withoutChat,
  withoutChatScopedParams,
  withParam,
} from './assistant-url'

const q = (s: string) => new URLSearchParams(s)

describe('readViewParam', () => {
  test('reads a known layout', () => {
    expect(readViewParam(q('view=work'))).toBe('work')
    expect(readViewParam(q('view=chats'))).toBe('chats')
  })

  test('ignores a missing or unknown layout', () => {
    expect(readViewParam(q(''))).toBeNull()
    expect(readViewParam(q('view=sideways'))).toBeNull()
  })
})

describe('withParam', () => {
  test('sets and removes without touching the original', () => {
    const base = q('chat=a&debug=true')
    expect(withParam(base, 'tab', 'preview').toString()).toBe(
      'chat=a&debug=true&tab=preview'
    )
    expect(withParam(base, 'chat', null).toString()).toBe('debug=true')
    expect(base.toString()).toBe('chat=a&debug=true')
  })

  test('treats an empty value as removal', () => {
    expect(withParam(q('tab=made'), 'tab', '').has('tab')).toBe(false)
  })
})

describe('withoutChat', () => {
  test('drops the chat and its tab state, keeps the rest', () => {
    const next = withoutChat(q('chat=a&view=work&tab=terminal&panel=open&x=1'))
    expect(next.toString()).toBe('view=work&x=1')
  })

  test('scoped variant keeps the chat', () => {
    expect(
      withoutChatScopedParams(q('chat=a&tab=made&panel=open')).toString()
    ).toBe('chat=a')
  })
})

describe('syncedPageParams', () => {
  test('is null when the address already says it', () => {
    expect(
      syncedPageParams(q('chat=a&view=work&tab=made'), {
        chat: 'a',
        view: 'work',
      })
    ).toBeNull()
  })

  test('keeps the tab when a reload resolves the chat the address names', () => {
    expect(
      syncedPageParams(q('chat=a&view=work&tab=terminal'), {
        chat: 'a',
        view: 'work',
      })
    ).toBeNull()
  })

  test('writes chat and layout together in one address', () => {
    expect(
      syncedPageParams(q(''), { chat: 'a', view: 'chats' })?.toString()
    ).toBe('chat=a&view=chats')
  })

  test('adds the layout to an address that has none', () => {
    expect(
      syncedPageParams(q('chat=a'), { chat: 'a', view: 'chats' })?.toString()
    ).toBe('chat=a&view=chats')
  })

  test('switching chats drops the previous chat tab state', () => {
    expect(
      syncedPageParams(q('chat=a&view=work&tab=terminal&panel=open&x=1'), {
        chat: 'b',
        view: 'work',
      })?.toString()
    ).toBe('chat=b&view=work&x=1')
  })

  test('does not invent a chat while none is open', () => {
    expect(
      syncedPageParams(q('view=work'), { chat: null, view: 'work' })
    ).toBeNull()
    expect(
      syncedPageParams(q(''), { chat: null, view: 'work' })?.toString()
    ).toBe('view=work')
  })
})
