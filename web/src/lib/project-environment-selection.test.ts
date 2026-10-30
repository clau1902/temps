// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'

import {
  ALL_ENVIRONMENTS,
  environmentLiveStatus,
  legacyEnvironmentSlug,
  resolveDefaultEnvironment,
  resolveEnvironmentSelection,
} from './project-environment-selection'

const staging = { id: 1, slug: 'staging', is_preview: false }
const production = { id: 2, slug: 'production', is_preview: false }
const preview = { id: 3, slug: 'pr-12', is_preview: true }
const environments = [staging, production, preview]

const select = (
  overrides: Partial<Parameters<typeof resolveEnvironmentSelection>[0]>
) =>
  resolveEnvironmentSelection({
    environments,
    urlParam: null,
    remembered: null,
    preferredDefault: null,
    allowAll: false,
    ...overrides,
  })

describe('resolveEnvironmentSelection', () => {
  test('defaults to production, not the first environment returned', () => {
    expect(select({})).toEqual({
      environment: production,
      all: false,
      source: 'fallback',
    })
  })

  test('the URL wins over the remembered and default environment', () => {
    const selection = select({
      urlParam: 'pr-12',
      remembered: 'staging',
      preferredDefault: 'staging',
    })
    expect(selection.environment).toBe(preview)
    expect(selection.source).toBe('url')
  })

  test('the remembered environment wins over the project default', () => {
    const selection = select({
      remembered: 'pr-12',
      preferredDefault: 'staging',
    })
    expect(selection.environment).toBe(preview)
    expect(selection.source).toBe('remembered')
  })

  test('uses the project default when nothing else is set', () => {
    const selection = select({ preferredDefault: 'staging' })
    expect(selection.environment).toBe(staging)
    expect(selection.source).toBe('default')
  })

  test('skips a slug that no longer exists', () => {
    const selection = select({ urlParam: 'deleted', remembered: 'staging' })
    expect(selection.environment).toBe(staging)
    expect(selection.source).toBe('remembered')
  })

  test('selects all environments only where the page allows it', () => {
    expect(select({ urlParam: ALL_ENVIRONMENTS, allowAll: true })).toEqual({
      environment: undefined,
      all: true,
      source: 'url',
    })
    const single = select({ urlParam: ALL_ENVIRONMENTS, allowAll: false })
    expect(single.all).toBe(false)
    expect(single.environment).toBe(production)
  })

  test('selects nothing for a project without environments', () => {
    const selection = resolveEnvironmentSelection({
      environments: [],
      urlParam: 'production',
      remembered: null,
      preferredDefault: null,
      allowAll: false,
    })
    expect(selection.environment).toBeUndefined()
    expect(selection.all).toBe(false)
  })
})

describe('resolveDefaultEnvironment', () => {
  test('prefers the stored default and falls back to production', () => {
    expect(resolveDefaultEnvironment(environments, 'staging')).toBe(staging)
    expect(resolveDefaultEnvironment(environments, 'gone')).toBe(production)
  })
})

describe('environmentLiveStatus', () => {
  test('distinguishes live, sleeping and never-deployed environments', () => {
    expect(
      environmentLiveStatus({ sleeping: false, current_deployment_id: 4 })
    ).toEqual({ tone: 'ok', label: 'Live' })
    expect(
      environmentLiveStatus({ sleeping: true, current_deployment_id: 4 })
    ).toEqual({ tone: 'idle', label: 'Sleeping' })
    expect(
      environmentLiveStatus({ sleeping: false, current_deployment_id: null })
    ).toEqual({ tone: 'idle', label: 'Not deployed' })
  })
})

test('a legacy ?environment= value is read as an id or a slug', () => {
  const environments = [
    { id: 3, slug: 'production' },
    { id: 5, slug: 'staging' },
  ]
  expect(legacyEnvironmentSlug(environments, '5')).toBe('staging')
  expect(legacyEnvironmentSlug(environments, 'production')).toBe('production')
  expect(legacyEnvironmentSlug(environments, '99')).toBeNull()
  expect(legacyEnvironmentSlug(environments, null)).toBeNull()
})
