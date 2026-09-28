// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { test, expect, describe } from 'bun:test'
import { parseCpuLimitCores } from './update.js'

describe('parseCpuLimitCores', () => {
  test('converts cores to the microcores the API stores', () => {
    // The API stores CPU in microcores (1_000_000 = one core). Sending the
    // core count unconverted would store `2` as two microcores.
    expect(parseCpuLimitCores('2')).toEqual({ microcores: 2_000_000 })
    expect(parseCpuLimitCores('1')).toEqual({ microcores: 1_000_000 })
    expect(parseCpuLimitCores('0.5')).toEqual({ microcores: 500_000 })
    expect(parseCpuLimitCores('0.25')).toEqual({ microcores: 250_000 })
  })

  test('rounds to a whole number of microcores', () => {
    expect(parseCpuLimitCores('0.1')).toEqual({ microcores: 100_000 })
    expect(parseCpuLimitCores('1.0000004')).toEqual({ microcores: 1_000_000 })
  })

  test('rejects non-numeric, zero, negative and trailing-junk values', () => {
    for (const value of ['abc', '0', '-1', '1abc', '1e3', 'Infinity']) {
      const result = parseCpuLimitCores(value)
      expect('error' in result && result.error).toContain('positive number of cores')
    }
  })

  test('rejects limits below Docker minimum of 0.01 cores', () => {
    expect(parseCpuLimitCores('0.01')).toEqual({ microcores: 10_000 })
    for (const value of ['0.005', '0.0000001']) {
      const result = parseCpuLimitCores(value)
      expect('error' in result && result.error).toContain('at least 0.01 cores')
    }
  })

  test('rejects implausibly large values', () => {
    expect(parseCpuLimitCores('256')).toEqual({ microcores: 256_000_000 })
    const result = parseCpuLimitCores('257')
    expect('error' in result && result.error).toContain('at most 256 cores')
  })
})
