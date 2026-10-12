// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'bun:test'
import { QueryClient } from '@tanstack/react-query'
import type { MonitorResponse, StatusPageResponse } from '@/api/client'
import {
  addDomainSchema,
  buildMonitorRows,
  domainStatusLabel,
  incidentUpdateSchema,
  initialIncidentStatus,
  MAX_INCIDENT_UPDATE_CHARS,
  operationQueries,
  statusPageFormSchema,
  toSelection,
} from './status-page-form'

function monitor(id: number, name: string): MonitorResponse {
  return {
    id,
    name,
    project_id: 1,
    environment_id: 1,
    monitor_type: 'web',
    monitor_url: `https://internal-${id}.example.test/health`,
    check_interval_seconds: 60,
    is_active: true,
    created_at: '2026-10-01T00:00:00Z',
    updated_at: '2026-10-01T00:00:00Z',
  }
}

const page = {
  monitors: [
    {
      monitor_id: 3,
      monitor_name: 'c-internal',
      display_name: 'Web',
      position: 1,
    },
    {
      monitor_id: 2,
      monitor_name: 'b-internal',
      display_name: 'API',
      position: 0,
    },
  ],
} as Pick<StatusPageResponse, 'monitors'>

describe('buildMonitorRows', () => {
  it('lists selected monitors first in page order, then the rest', () => {
    const rows = buildMonitorRows(
      [
        monitor(1, 'a-internal'),
        monitor(2, 'b-internal'),
        monitor(3, 'c-internal'),
      ],
      page
    )
    expect(rows.map((r) => [r.monitor_id, r.selected, r.display_name])).toEqual(
      [
        [2, true, 'API'],
        [3, true, 'Web'],
        [1, false, ''],
      ]
    )
  })

  it('never pre-fills an internal name as the public name', () => {
    const rows = buildMonitorRows([monitor(1, 'db-primary.internal')])
    expect(rows[0].display_name).toBe('')
    expect(rows[0].monitor_name).toBe('db-primary.internal')
  })
})

describe('toSelection', () => {
  it('sends only selected monitors with trimmed public names', () => {
    expect(
      toSelection([
        {
          monitor_id: 1,
          monitor_name: 'x',
          selected: true,
          display_name: ' API ',
        },
        {
          monitor_id: 2,
          monitor_name: 'y',
          selected: false,
          display_name: 'Web',
        },
      ])
    ).toEqual([{ monitor_id: 1, display_name: 'API' }])
  })
})

describe('statusPageFormSchema', () => {
  it('requires a public name for every selected monitor', () => {
    const result = statusPageFormSchema.safeParse({
      title: 'Status',
      description: '',
      monitors: [
        {
          monitor_id: 1,
          monitor_name: 'x',
          selected: true,
          display_name: '  ',
        },
        { monitor_id: 2, monitor_name: 'y', selected: false, display_name: '' },
      ],
    })
    expect(result.success).toBe(false)
    expect(result.error?.issues[0].path).toEqual([
      'monitors',
      0,
      'display_name',
    ])
  })

  it('accepts a valid page', () => {
    expect(
      statusPageFormSchema.safeParse({
        title: 'Status',
        description: '',
        monitors: [
          {
            monitor_id: 1,
            monitor_name: 'x',
            selected: true,
            display_name: 'API',
          },
        ],
      }).success
    ).toBe(true)
  })
})

describe('operationQueries', () => {
  it('matches every cached variant of a generated SDK query key', async () => {
    const client = new QueryClient()
    const detail = [
      { _id: 'getStatusPage', baseUrl: '/api', path: { status_page_id: 1 } },
    ]
    const other = [
      { _id: 'listStatusPages', baseUrl: '/api', path: { project_id: 1 } },
    ]
    client.setQueryData(detail, { id: 1 })
    client.setQueryData(other, { items: [] })
    await client.invalidateQueries(operationQueries('getStatusPage'))
    expect(client.getQueryState(detail)?.isInvalidated).toBe(true)
    expect(client.getQueryState(other)?.isInvalidated).toBe(false)
  })
})

describe('incidentUpdateSchema', () => {
  it('requires a message and enforces the server limit', () => {
    const base = { status: 'identified' as const, is_public: true }
    expect(
      incidentUpdateSchema.safeParse({ ...base, message: '  ' }).success
    ).toBe(false)
    expect(
      incidentUpdateSchema.safeParse({
        ...base,
        message: 'x'.repeat(MAX_INCIDENT_UPDATE_CHARS + 1),
      }).success
    ).toBe(false)
    expect(
      incidentUpdateSchema.safeParse({
        ...base,
        message: 'x'.repeat(MAX_INCIDENT_UPDATE_CHARS),
      }).success
    ).toBe(true)
  })

  it('falls back to investigating for an unknown incident status', () => {
    expect(initialIncidentStatus('monitoring')).toBe('monitoring')
    expect(initialIncidentStatus('something-else')).toBe('investigating')
  })
})

describe('addDomainSchema', () => {
  it('accepts a hostname and refuses URLs, wildcards and bare labels', () => {
    const ok = (hostname: string) =>
      addDomainSchema.safeParse({ hostname, verification_method: 'http-01' })
        .success
    expect(ok('status.example.com')).toBe(true)
    expect(ok('status.example.com.')).toBe(true)
    expect(ok('https://status.example.com')).toBe(false)
    expect(ok('status.example.com/path')).toBe(false)
    expect(ok('*.example.com')).toBe(false)
    expect(ok('localhost')).toBe(false)
    expect(ok('')).toBe(false)
  })
})

describe('domainStatusLabel', () => {
  it('distinguishes serving from verified-but-unpublished', () => {
    expect(domainStatusLabel({ status: 'active', serving: true })).toBe(
      'Serving'
    )
    expect(domainStatusLabel({ status: 'active', serving: false })).toContain(
      'unpublished'
    )
    expect(domainStatusLabel({ status: 'pending', serving: false })).toBe(
      'Waiting for DNS'
    )
  })
})
