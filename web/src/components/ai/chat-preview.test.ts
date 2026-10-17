// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { canEmbed, parsePort, previewUrlFor } from './chat-preview'

describe('parsePort', () => {
  test('accepts whole port numbers, ignoring surrounding space', () => {
    expect(parsePort('3000')).toBe(3000)
    expect(parsePort(' 8080 ')).toBe(8080)
    expect(parsePort('65535')).toBe(65535)
    expect(parsePort('1')).toBe(1)
  })

  test('rejects anything that is not a port', () => {
    for (const text of [
      '',
      '0',
      '65536',
      '-1',
      '30.5',
      'abc',
      '3000a',
      '1e3',
    ]) {
      expect(parsePort(text)).toBeNull()
    }
  })
})

describe('previewUrlFor', () => {
  test('puts the port into the template', () => {
    expect(previewUrlFor('http://ws-abc-{port}.localho.st:8080', 5173)).toBe(
      'http://ws-abc-5173.localho.st:8080'
    )
  })
})

describe('canEmbed', () => {
  test('refuses an http page inside an https console only', () => {
    expect(canEmbed('http://ws-a-1.localho.st:8080', 'https:')).toBe(false)
    expect(canEmbed('https://ws-a-1.example.test', 'https:')).toBe(true)
    expect(canEmbed('http://ws-a-1.localho.st:8080', 'http:')).toBe(true)
  })
})
