// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { BreadcrumbProvider } from '@/contexts/BreadcrumbContext'
import { jsonResponse, mockApi } from '@/test/fetch-mock'
import { IngressSetupPage } from './IngressSetupPage'

afterEach(cleanup)

const worker = (id: number, name: string, status = 'active') => ({
  id,
  name,
  status,
  role: 'worker',
  address: '10.0.0.' + id,
  private_address: '10.0.0.' + id,
  created_at: '2026-01-01T00:00:00Z',
  last_heartbeat: '2026-01-01T00:00:00Z',
  labels: {},
  capacity: {},
  build_limits: {
    concurrency: 1,
    queue_wait_seconds: 300,
    build_timeout_seconds: 1800,
  },
  public_ingress_enabled: false,
  public_ingress_unsupported_reasons: [],
})

const settings = (nodeId: number, mode: string) => ({
  node_id: nodeId,
  mode,
  agent_reports: true,
  behind_nat: false,
  enabled: false,
  http_port: 80,
  https_port: 443,
  interfaces: [],
})

const tunnel = (state: string) => ({
  id: 9,
  name: 'example-com',
  provider: 'cloudflare_tunnel',
  domain: 'example.com',
  status: 'ready',
  auto_publish: true,
  edge_port: 8080,
  created_at: '2026-01-01T00:00:00Z',
  publications: [],
  nodes: [{ node_id: 1, node_name: 'worker-a', state, ready_connections: 4 }],
})

function setup() {
  const posted: { path: string; body: unknown }[] = []
  const api = mockApi({
    'GET /internal/nodes': () =>
      jsonResponse({
        nodes: [
          worker(1, 'worker-a'),
          worker(2, 'worker-b'),
          worker(3, 'worker-c', 'offline'),
        ],
      }),
    'GET /internal/nodes/1/public-ingress/settings': () =>
      jsonResponse(settings(1, 'delegated')),
    'GET /internal/nodes/2/public-ingress/settings': () =>
      jsonResponse(settings(2, 'off')),
    'GET /ingress/providers': () =>
      jsonResponse([
        {
          kind: 'cloudflare_tunnel',
          name: 'Cloudflare Tunnel',
          summary: '',
          requirements: [],
          fields: [],
          needs_public_ip: false,
          works_behind_cgnat: true,
          dns: 'automatic',
        },
      ]),
    'GET /ingress/connectors': () =>
      jsonResponse({ connectors: [], total: 0, page: 1, page_size: 100 }),
    'GET /dns-providers': () =>
      jsonResponse([
        {
          id: 1,
          name: 'Cloudflare',
          provider_type: 'cloudflare',
          is_active: true,
          credentials: {},
          flat_hostnames_supported: true,
          created_at: '2026-01-01T00:00:00Z',
          updated_at: '2026-01-01T00:00:00Z',
        },
      ]),
    'GET /dns-providers/1/zones': () =>
      jsonResponse({
        zones: [
          { id: 'z1', name: 'example.com', status: 'active', nameservers: [] },
        ],
      }),
    'GET /dns-providers/1/domains': () => jsonResponse([]),
    'POST /dns-providers/1/domains': async (request) => {
      posted.push({ path: 'domains', body: await request.json() })
      return jsonResponse(managedDomain(false), 201)
    },
    'POST /dns-providers/1/domains/example.com/verify': () =>
      jsonResponse(managedDomain(true)),
    'POST /ingress/connectors': async (request) => {
      posted.push({ path: 'connectors', body: await request.json() })
      return jsonResponse(tunnel('pending'), 201)
    },
    'GET /ingress/connectors/9': () => jsonResponse(tunnel('ready')),
  })
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={['/settings/nodes/ingress']}>
        <BreadcrumbProvider>
          <IngressSetupPage />
        </BreadcrumbProvider>
      </MemoryRouter>
    </QueryClientProvider>
  )
  return { api, posted }
}

function managedDomain(verified: boolean) {
  return {
    id: 3,
    provider_id: 1,
    domain: 'example.com',
    auto_manage: true,
    verified,
    proxied_by_default: false,
    sync_generated_records: false,
    generated_hostname_mode: 'standard',
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
}

test('defaults to a tunnel on the workers with no way in, and sets it up step by step', async () => {
  const { posted } = setup()
  await screen.findByText('example.com')
  // Recommended: no worker has a public address.
  expect(screen.getByText('Recommended')).toBeTruthy()

  const a = screen.getByRole('checkbox', { name: 'worker-a' })
  const b = screen.getByRole('checkbox', { name: 'worker-b' })
  const c = screen.getByRole('checkbox', { name: 'worker-c' })
  await waitFor(() => expect(a.getAttribute('data-state')).toBe('checked'))
  // B refuses public traffic, C is offline: neither can be picked.
  await waitFor(() => expect(b.hasAttribute('disabled')).toBe(true))
  expect(c.hasAttribute('disabled')).toBe(true)
  expect(screen.getByText('temps agent ingress delegated')).toBeTruthy()

  fireEvent.click(screen.getByRole('radio', { name: /example\.com/ }))
  // The subdomain scheme warns about Universal SSL before it can be used.
  fireEvent.click(screen.getByRole('radio', { name: /Under a subdomain/ }))
  expect(
    screen.getByText('HTTPS fails without Advanced Certificate Manager')
  ).toBeTruthy()
  fireEvent.click(screen.getByRole('radio', { name: /app\.example\.com/ }))

  const create = screen.getByRole('button', { name: 'Create tunnel' })
  await waitFor(() => expect(create.hasAttribute('disabled')).toBe(false))
  fireEvent.click(create)

  await screen.findByText('Visitors can reach apps under example.com')
  expect(screen.getByText('Connected on worker-a')).toBeTruthy()
  expect(screen.getByText('4 connections to Cloudflare')).toBeTruthy()
  expect(posted).toEqual([
    { path: 'domains', body: { domain: 'example.com', auto_manage: true } },
    {
      path: 'connectors',
      body: {
        name: 'example-com',
        provider: 'cloudflare_tunnel',
        values: { dns_provider_id: 1, domain: 'example.com' },
        node_ids: [1],
      },
    },
  ])
})

test('open ports hands over to each worker’s own page', async () => {
  setup()
  fireEvent.click(
    await screen.findByRole('radio', { name: /Open ports on a public IP/ })
  )
  const link = await screen.findByRole('link', {
    name: 'Set up open ports on worker-a',
  })
  expect(link.getAttribute('href')).toBe('/settings/nodes/1#public-ingress')
})
