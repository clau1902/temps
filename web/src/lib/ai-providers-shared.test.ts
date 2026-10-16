// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { providerFromAddParam } from './ai-providers-shared'

describe('providerFromAddParam', () => {
  test('resolves a supported provider id from the deep link', () => {
    expect(providerFromAddParam('openrouter')?.name).toBe('OpenRouter')
  })

  test('ignores missing, unknown and inherited keys', () => {
    expect(providerFromAddParam(null)).toBeUndefined()
    expect(providerFromAddParam('')).toBeUndefined()
    expect(providerFromAddParam('not-a-provider')).toBeUndefined()
    expect(providerFromAddParam('constructor')).toBeUndefined()
    expect(providerFromAddParam('__proto__')).toBeUndefined()
  })
})
