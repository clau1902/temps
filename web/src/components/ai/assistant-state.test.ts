// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  ASSISTANT_EXAMPLE_PROMPTS,
  NO_PROVIDER_REASON,
  OPENROUTER_SETUP_PATH,
  ASK_BEFORE_CHANGES_MODE,
  RUN_CHANGES_AUTOMATICALLY_MODE,
  assistantReadiness,
  asksBeforeChanges,
  isAssistantConversation,
  loadDetailLevel,
  permissionModeForAsk,
  saveDetailLevel,
  setupPathFromProblem,
} from './assistant-state'

describe('assistantReadiness', () => {
  test('is ready when a gateway provider is available', () => {
    expect(assistantReadiness({ gateway_available: true })).toEqual({
      state: 'ready',
    })
  })

  test('a ready host harness without a gateway key does not count', () => {
    const readiness = assistantReadiness({
      gateway_available: false,
      reason: null,
      setup_path: null,
    })
    expect(readiness.state).toBe('unconfigured')
  })

  test('uses the server reason and setup path when they are usable', () => {
    expect(
      assistantReadiness({
        gateway_available: false,
        reason: 'No AI provider is configured.',
        setup_path: '/ai-gateway?add=openrouter',
      })
    ).toEqual({
      state: 'unconfigured',
      reason: 'No AI provider is configured.',
      setupPath: '/ai-gateway?add=openrouter',
    })
  })

  test('falls back to the default reason and OpenRouter path when omitted', () => {
    expect(assistantReadiness(undefined)).toEqual({
      state: 'unconfigured',
      reason: NO_PROVIDER_REASON,
      setupPath: OPENROUTER_SETUP_PATH,
    })
    expect(
      assistantReadiness({ gateway_available: false, reason: '   ' })
    ).toMatchObject({ reason: NO_PROVIDER_REASON })
  })

  test('refuses an off-console setup path rather than rendering it as a link', () => {
    for (const setup_path of [
      'https://evil.example/ai',
      '//evil.example/ai',
      'javascript:alert(1)',
    ]) {
      expect(
        assistantReadiness({ gateway_available: false, setup_path })
      ).toMatchObject({ setupPath: OPENROUTER_SETUP_PATH })
    }
  })

  test('the default reason names the gap in the required words', () => {
    expect(NO_PROVIDER_REASON).toContain('No AI provider is configured')
  })
})

describe('setupPathFromProblem', () => {
  test('reads a console path from a 409 problem document', () => {
    expect(
      setupPathFromProblem({ status: 409, setup_path: '/ai-gateway?add=x' })
    ).toBe('/ai-gateway?add=x')
  })

  test('ignores missing, malformed and external paths', () => {
    expect(setupPathFromProblem(undefined)).toBeUndefined()
    expect(setupPathFromProblem('nope')).toBeUndefined()
    expect(setupPathFromProblem({ setup_path: 3 })).toBeUndefined()
    expect(
      setupPathFromProblem({ setup_path: 'https://evil.example' })
    ).toBeUndefined()
  })
})

describe('isAssistantConversation', () => {
  test('matches only the assistant context type', () => {
    expect(isAssistantConversation({ context_type: 'assistant' })).toBe(true)
    expect(isAssistantConversation({ context_type: 'global' })).toBe(false)
    expect(isAssistantConversation({ context_type: 'project' })).toBe(false)
  })
})

describe('detail level preference', () => {
  function memoryStorage() {
    const data = new Map<string, string>()
    return {
      getItem: (key: string) => data.get(key) ?? null,
      setItem: (key: string, value: string) => void data.set(key, value),
      removeItem: (key: string) => void data.delete(key),
    }
  }

  test('defaults to simple and round-trips advanced', () => {
    const storage = memoryStorage()
    expect(loadDetailLevel(storage)).toBe('simple')
    saveDetailLevel('advanced', storage)
    expect(loadDetailLevel(storage)).toBe('advanced')
    saveDetailLevel('simple', storage)
    expect(loadDetailLevel(storage)).toBe('simple')
  })

  test('survives storage that throws', () => {
    const broken = {
      getItem: () => {
        throw new Error('denied')
      },
      setItem: () => {
        throw new Error('denied')
      },
      removeItem: () => {
        throw new Error('denied')
      },
    }
    expect(loadDetailLevel(broken)).toBe('simple')
    expect(() => saveDetailLevel('advanced', broken)).not.toThrow()
  })
})

describe('example prompts', () => {
  test('are concrete, non-empty questions', () => {
    expect(ASSISTANT_EXAMPLE_PROMPTS.length).toBeGreaterThan(0)
    for (const prompt of ASSISTANT_EXAMPLE_PROMPTS) {
      expect(prompt.trim().length).toBeGreaterThan(10)
    }
  })
})

describe('ask before changes', () => {
  test('only the confirming mode asks', () => {
    expect(asksBeforeChanges(ASK_BEFORE_CHANGES_MODE)).toBe(true)
    expect(asksBeforeChanges(RUN_CHANGES_AUTOMATICALLY_MODE)).toBe(false)
    expect(asksBeforeChanges(null)).toBe(false)
  })

  test('round-trips through the permission mode ids the server accepts', () => {
    expect(permissionModeForAsk(true)).toBe('confirm-actions')
    expect(permissionModeForAsk(false)).toBe('auto')
    expect(asksBeforeChanges(permissionModeForAsk(true))).toBe(true)
    expect(asksBeforeChanges(permissionModeForAsk(false))).toBe(false)
  })
})
