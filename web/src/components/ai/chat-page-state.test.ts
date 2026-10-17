// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'

import {
  chatComposerSubmitAction,
  conversationListNeedsRefresh,
  createProjectChat,
  hasProjectScope,
  isChatComposerDisabled,
  CHAT_COMPOSER_COMPACT_MIN_HEIGHT,
  enterSendsMessage,
  resolveChatComposerLayout,
  resolvePageChat,
} from './chat-page-state'

describe('hasProjectScope', () => {
  test('keeps classic project chats and excludes user-owned application chats', () => {
    const items = [
      { public_id: 'project', project_id: 42 },
      { public_id: 'application', project_id: null },
    ]

    expect(items.filter(hasProjectScope)).toEqual([
      { public_id: 'project', project_id: 42 },
    ])
  })
})

const conversations = [
  { public_id: 'most-recent' },
  { public_id: 'older-chat' },
]

describe('resolvePageChat', () => {
  test('keeps the conversation selected by the URL across reloads', () => {
    expect(resolvePageChat('older-chat', conversations)?.public_id).toBe(
      'older-chat'
    )
  })

  test('does not resume a conversation on a bare chat route', () => {
    expect(resolvePageChat(null, conversations)).toBeNull()
  })

  test('does not substitute another conversation for a stale URL', () => {
    expect(resolvePageChat('deleted-chat', conversations)).toBeNull()
  })

  test('is empty when there are no conversations', () => {
    expect(resolvePageChat('older-chat', [])).toBeNull()
  })
})

describe('createProjectChat', () => {
  test('creates a fresh active chat instead of opening the project picker', () => {
    expect(
      createProjectChat(
        { id: 42, slug: 'example-project', name: 'Example project' },
        'fresh-context-id'
      )
    ).toEqual({
      projectId: 42,
      projectSlug: 'example-project',
      projectName: 'Example project',
      contextType: 'project',
      contextId: 'fresh-context-id',
      title: 'Project chat',
      autoStart: false,
    })
  })
})

describe('conversationListNeedsRefresh', () => {
  test('refreshes the sidebar when the first message creates a new chat', () => {
    expect(conversationListNeedsRefresh('new-chat', conversations)).toBe(true)
  })

  test('does not refetch for a conversation already shown in the sidebar', () => {
    expect(conversationListNeedsRefresh('older-chat', conversations)).toBe(
      false
    )
  })

  test('does not refetch while the new chat is still an unsaved draft', () => {
    expect(conversationListNeedsRefresh(null, conversations)).toBe(false)
  })
})

describe('resolveChatComposerLayout', () => {
  test('keeps a short draft at the compact minimum height', () => {
    expect(resolveChatComposerLayout(28)).toEqual({
      height: 72,
      overflowY: 'hidden',
    })
  })

  test('rests at one line on a phone', () => {
    expect(
      resolveChatComposerLayout(28, CHAT_COMPOSER_COMPACT_MIN_HEIGHT)
    ).toEqual({ height: 40, overflowY: 'hidden' })
    expect(
      resolveChatComposerLayout(90, CHAT_COMPOSER_COMPACT_MIN_HEIGHT).height
    ).toBe(90)
  })

  test('grows with multiline content', () => {
    expect(resolveChatComposerLayout(156)).toEqual({
      height: 156,
      overflowY: 'hidden',
    })
  })

  test('caps long drafts and enables an internal scrollbar', () => {
    expect(resolveChatComposerLayout(420)).toEqual({
      height: 240,
      overflowY: 'auto',
    })
  })
})

describe('chat composer interruption', () => {
  test('keeps an existing chat editable while a response streams', () => {
    expect(isChatComposerDisabled(true, false, false)).toBe(false)
    expect(chatComposerSubmitAction('change direction', true)).toBe(
      'interrupt-and-send'
    )
  })

  test('does not submit an empty interruption', () => {
    expect(chatComposerSubmitAction('   ', true)).toBe('none')
  })
})

describe('enterSendsMessage', () => {
  const enter = { key: 'Enter', shiftKey: false }

  test('sends on a computer', () => {
    expect(enterSendsMessage(enter, false)).toBe(true)
  })

  test('shift+enter and other keys never send', () => {
    expect(enterSendsMessage({ ...enter, shiftKey: true }, false)).toBe(false)
    expect(enterSendsMessage({ key: 'a', shiftKey: false }, false)).toBe(false)
  })

  test('confirming an IME candidate never sends', () => {
    expect(enterSendsMessage({ ...enter, isComposing: true }, false)).toBe(
      false
    )
  })

  test('is a new line on a phone', () => {
    expect(enterSendsMessage(enter, true)).toBe(false)
  })
})
