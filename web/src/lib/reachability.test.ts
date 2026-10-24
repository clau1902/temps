// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import type {
  AddressReachability,
  EnvironmentReachability,
  RunView,
  SetupField,
} from '@/api/client'
import {
  applyValues,
  canResumeRun,
  canUndoRun,
  resumeNeedsToken,
  stalePlan,
  certificateChoices,
  defaultOption,
  missingAnswers,
  reachabilityHeadline,
  wayInLabel,
} from './reachability'

const address = (
  overrides: Partial<AddressReachability>
): AddressReachability => ({
  address: 'app.example.com',
  audience: 'public',
  source: 'custom_domain',
  state: 'not_reachable',
  reason: 'No DNS record for app.example.com exists yet.',
  ...overrides,
})

const environment = (
  overrides: Partial<EnvironmentReachability>
): EnvironmentReachability => ({
  project_id: 1,
  environment_id: 2,
  environment_name: 'production',
  deployed: true,
  running_on: ['worker-1'],
  state: 'not_reachable',
  summary: 'It runs on worker-1, which has no way in.',
  addresses: [],
  plan: null,
  ...overrides,
})

describe('reachability headline', () => {
  test('names the reachable address, a custom domain before the generated one', () => {
    const headline = reachabilityHeadline(
      environment({
        state: 'reachable',
        addresses: [
          address({
            address: 'production.example.org',
            source: 'generated',
            state: 'reachable',
          }),
          address({ address: 'shop.example.com', state: 'reachable' }),
        ],
      })
    )
    expect(headline).toEqual({
      kind: 'public',
      address: 'shop.example.com',
      url: 'https://shop.example.com',
    })
  })

  test('says why not, and offers the fix only when the plan has an option', () => {
    const withPlan = reachabilityHeadline(
      environment({
        plan: {
          audience: 'public',
          options: [
            {
              kind: 'new_tunnel_with_token',
              title: 'Through a new Cloudflare Tunnel',
              description: '',
              actions: 3,
              recommended: true,
              way_in: 'tunnel:new',
              steps: [],
              fields: [],
              warnings: [],
              required_permissions: [],
            },
          ],
        },
      })
    )
    expect(withPlan).toEqual({
      kind: 'not_reachable',
      reason: 'It runs on worker-1, which has no way in.',
      canFix: true,
    })
    const blocked = reachabilityHeadline(environment({ plan: null }))
    expect(blocked.kind === 'not_reachable' && blocked.canFix).toBe(false)
  })

  test('an address Temps cannot judge is unknown, not broken', () => {
    expect(
      reachabilityHeadline(
        environment({ state: 'unknown', summary: 'DNS points elsewhere.' })
      )
    ).toEqual({ kind: 'unknown', reason: 'DNS points elsewhere.' })
  })
})

test('ways in read as words', () => {
  expect(wayInLabel('tunnel:homelab')).toBe('Tunnel homelab')
  expect(wayInLabel('direct:edge-1')).toBe('Straight to edge-1')
  expect(wayInLabel('direct:control-plane')).toBe('This server')
  expect(wayInLabel(null)).toBe('No way in yet')
})

describe('runs', () => {
  const run = (overrides: Partial<RunView>): RunView => ({
    id: 1,
    audience: 'public',
    option: 'existing_tunnel',
    state: 'succeeded',
    steps: [],
    created: [{ type: 'custom_domain', id: 4, domain: 'app.example.com' }],
    journal: [],
    undo: [],
    created_at: '',
    updated_at: '',
    ...overrides,
  })

  test('undo is offered while something it created is left', () => {
    expect(canUndoRun(run({}))).toBe(true)
    expect(canUndoRun(run({ state: 'running' }))).toBe(false)
    expect(canUndoRun(run({ state: 'undone' }))).toBe(false)
    expect(canUndoRun(run({ created: [] }))).toBe(false)
    // Recovering after a restart: wait until Temps has checked.
    expect(canUndoRun(run({ state: 'recovering' }))).toBe(false)
    expect(
      canUndoRun(
        run({
          state: 'interrupted',
          journal: [
            {
              key: 'run-1/connector/t',
              intent: { type: 'connector', name: 't' },
              state: 'pending',
              at: '',
            },
          ],
        })
      )
    ).toBe(false)
    // What undo removed leaves the journal's done entries.
    expect(
      canUndoRun(
        run({
          state: 'failed',
          created: [],
          undo: [
            {
              resource: {
                type: 'custom_domain',
                id: 4,
                domain: 'app.example.com',
              },
              label: 'Domain app.example.com',
              ok: true,
            },
          ],
        })
      )
    ).toBe(false)
  })
})

test('waiting and interrupted runs resume, asking for the token only when needed', () => {
  const base: RunView = {
    id: 2,
    audience: 'public',
    option: 'new_tunnel_with_token',
    state: 'interrupted',
    steps: [
      { kind: 'check_token', label: 'Check', status: 'done' },
      { kind: 'save_dns_provider', label: 'Save', status: 'pending' },
    ],
    created: [],
    journal: [],
    undo: [],
    created_at: '',
    updated_at: '',
  }
  expect(canResumeRun(base)).toBe(true)
  expect(resumeNeedsToken(base)).toBe(true)
  expect(canResumeRun({ ...base, state: 'failed' })).toBe(false)
  expect(canResumeRun({ ...base, state: 'recovering' })).toBe(false)
  expect(
    resumeNeedsToken({
      ...base,
      steps: [{ kind: 'save_dns_provider', label: 'Save', status: 'done' }],
    })
  ).toBe(false)
})

test('a refused apply hands back the new plan', () => {
  const plan = { audience: 'public', options: [], plan_id: 'p2' }
  expect(stalePlan({ status: 409, detail: 'changed', plan })).toEqual(
    plan as never
  )
  expect(stalePlan({ status: 409, detail: 'busy' })).toBeNull()
  expect(stalePlan({ plan: { options: 'no' } })).toBeNull()
  expect(stalePlan(new Error('network'))).toBeNull()
})

describe('apply answers', () => {
  const fields: SetupField[] = [
    {
      key: 'address',
      label: 'Address',
      kind: { type: 'text' },
      required: true,
    },
    {
      key: 'dns_provider_id',
      label: 'Cloudflare account',
      kind: { type: 'dns_provider', provider_type: 'cloudflare' },
      required: true,
    },
    {
      key: 'cloudflare_api_token',
      label: 'API token',
      kind: { type: 'secret' },
      required: false,
    },
  ]

  test('carry provider ids as numbers and leave blanks out', () => {
    expect(
      applyValues(
        fields,
        {
          address: ' app.example.com ',
          dns_provider_id: '7',
          cloudflare_api_token: '',
        },
        null
      )
    ).toEqual({ address: 'app.example.com', dns_provider_id: 7 })
  })

  test('send the planned address when the option does not ask for one', () => {
    expect(applyValues([], {}, 'app.example.com')).toEqual({
      address: 'app.example.com',
    })
  })

  test('name what is still missing', () => {
    expect(missingAnswers(fields, { address: 'x.example.com' })).toEqual([
      'Cloudflare account',
    ])
  })

  test('the recommended option is preselected', () => {
    const option = (recommended: boolean, title: string) => ({
      kind: 'existing_tunnel' as const,
      title,
      description: '',
      actions: 1,
      recommended,
      way_in: '',
      steps: [],
      fields: [],
      warnings: [],
      required_permissions: [],
    })
    expect(defaultOption([option(false, 'a'), option(true, 'b')])?.title).toBe(
      'b'
    )
    expect(defaultOption([option(false, 'a')])?.title).toBe('a')
    expect(defaultOption([])).toBeUndefined()
  })
})

describe('certificate choices', () => {
  test('HTTP-01 is recommended only when a direct ingress takes port 80', () => {
    expect(
      certificateChoices({
        coveringTunnels: 0,
        wildcard: false,
        http01Available: true,
      })
    ).toEqual({ showHttp01: true, recommended: 'http-01', http01Note: null })
    const noPort80 = certificateChoices({
      coveringTunnels: 0,
      wildcard: false,
      http01Available: false,
    })
    expect(noPort80.showHttp01).toBe(false)
    expect(noPort80.recommended).toBe('dns-01')
    expect(noPort80.http01Note).toContain('port 80')
  })

  test('while Temps cannot tell, HTTP-01 is listed but not recommended', () => {
    expect(
      certificateChoices({
        coveringTunnels: 0,
        wildcard: false,
        http01Available: undefined,
      })
    ).toEqual({ showHttp01: true, recommended: null, http01Note: null })
  })

  test('a covering tunnel wins, and wildcards need DNS-01', () => {
    expect(
      certificateChoices({
        coveringTunnels: 1,
        wildcard: false,
        http01Available: true,
      }).recommended
    ).toBe('tunnel')
    expect(
      certificateChoices({
        coveringTunnels: 0,
        wildcard: true,
        http01Available: true,
      })
    ).toEqual({ showHttp01: false, recommended: 'dns-01', http01Note: null })
  })
})
