// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { parseMonitorSpec, parseMonitorSpecs } from './index.js'

describe('parseMonitorSpec', () => {
  test('parses <id>=<public name>', () => {
    expect(parseMonitorSpec('12=Public API')).toEqual({
      monitor_id: 12,
      display_name: 'Public API',
    })
  })

  test('keeps "=" inside the public name', () => {
    expect(parseMonitorSpec('3=a=b')).toEqual({ monitor_id: 3, display_name: 'a=b' })
  })

  test('rejects a missing name, bad id or missing separator', () => {
    expect(typeof parseMonitorSpec('12')).toBe('string')
    expect(typeof parseMonitorSpec('12=  ')).toBe('string')
    expect(typeof parseMonitorSpec('abc=API')).toBe('string')
    expect(typeof parseMonitorSpec('-1=API')).toBe('string')
    expect(typeof parseMonitorSpec('=API')).toBe('string')
  })
})

describe('parseMonitorSpecs', () => {
  test('keeps order and stops at the first invalid spec', () => {
    expect(parseMonitorSpecs(['2=Web', '1=API'])).toEqual([
      { monitor_id: 2, display_name: 'Web' },
      { monitor_id: 1, display_name: 'API' },
    ])
    expect(typeof parseMonitorSpecs(['2=Web', 'bad'])).toBe('string')
    expect(parseMonitorSpecs(undefined)).toEqual([])
  })
})
