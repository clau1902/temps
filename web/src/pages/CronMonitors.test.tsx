// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import {
  act,
  cleanup,
  fireEvent,
  render,
  waitFor,
} from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { installBrowserWindow } from '@/test/dom'
import { client as apiClient } from '@/api/client/client.gen'
import { getEnvironmentsOptions } from '@/api/client/@tanstack/react-query.gen'
import type { ProjectResponse } from '@/api/client'
import {
  ProjectEnvironmentContext,
  type ProjectEnvironmentContextValue,
} from '@/components/project-frame/project-environment-context'
import CronMonitors from './CronMonitors'

installBrowserWindow()
const original = apiClient.getConfig()
afterEach(() => {
  cleanup()
  apiClient.setConfig(original)
})

test('cron limits validate before transport; a native API error preserves inputs for retry', async () => {
  const requests: Record<string, unknown>[] = []
  apiClient.setConfig({
    baseUrl: 'https://console.example.test/api',
    fetch: Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const request =
          input instanceof Request ? input : new Request(input, init)
        if (request.method === 'POST') {
          requests.push(await request.json())
          return Response.json(
            {
              title: 'Service Unavailable',
              status: 503,
              detail: 'Synthetic monitor storage unavailable',
            },
            { status: 503 }
          )
        }
        return Response.json([])
      },
      { preconnect: fetch.preconnect }
    ),
  })
  const query = new QueryClient({
    defaultOptions: {
      queries: { retry: false, staleTime: Infinity },
      mutations: { retry: false },
    },
  })
  query.setQueryData(
    getEnvironmentsOptions({ path: { project_id: 17 } }).queryKey,
    []
  )
  const project = {
    id: 17,
    slug: 'synthetic',
    name: 'Synthetic',
  } as ProjectResponse
  const view = render(
    <QueryClientProvider client={query}>
      <MemoryRouter initialEntries={['/new']}>
        <CronMonitors project={project} />
      </MemoryRouter>
    </QueryClientProvider>
  )
  const form = view.getByLabelText('Monitor slug').closest('form')!
  fireEvent.change(view.getByLabelText('Monitor slug'), {
    target: { value: 'synthetic-job' },
  })
  fireEvent.change(view.getByLabelText('Failures before alert'), {
    target: { value: '101' },
  })
  await act(async () => {
    fireEvent.submit(form)
  })
  expect(requests.length).toBe(0)
  expect(view.container.textContent).toContain(
    'Enter a whole number from 1 to 100.'
  )
  fireEvent.change(view.getByLabelText('Failures before alert'), {
    target: { value: '2' },
  })
  await act(async () => {
    fireEvent.submit(form)
  })
  await waitFor(() =>
    expect(view.container.textContent).toContain(
      'Synthetic monitor storage unavailable'
    )
  )
  expect(requests.length).toBe(1)
  expect(requests[0]).toEqual({
    slug: 'synthetic-job',
    environment_id: null,
    config: {
      schedule: { type: 'crontab', value: '0 * * * *' },
      timezone: 'UTC',
      checkin_margin: 1,
      max_runtime: 30,
      failure_issue_threshold: 2,
      recovery_threshold: 1,
    },
  })
  expect((view.getByLabelText('Monitor slug') as HTMLInputElement).value).toBe(
    'synthetic-job'
  )
  await act(async () => {
    fireEvent.submit(form)
  })
  await waitFor(() => expect(requests.length).toBe(2))
  query.clear()
})

/** Serves the cron list page's reads; records every path requested. */
function serveCronList(paths: string[]) {
  apiClient.setConfig({
    baseUrl: 'https://console.example.test/api',
    fetch: Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const request =
          input instanceof Request ? input : new Request(input, init)
        const path = new URL(request.url).pathname.replace(/^\/api/, '')
        paths.push(path)
        const crons = path.match(/^\/projects\/17\/environments\/(\d+)\/crons$/)
        if (crons)
          return Response.json([
            {
              id: 9,
              path: `/api/tick-${crons[1]}`,
              schedule: '*/10 * * * *',
              environment_id: Number(crons[1]),
              project_id: 17,
              created_at: '2026-01-01T00:00:00Z',
              next_run: null,
              mode: 'http',
              concurrency_policy: 'allow',
              retry_limit: 0,
            },
          ])
        if (path === '/projects/17/environments')
          return Response.json([
            { id: 3, name: 'production', slug: 'production' },
          ])
        if (path === '/projects/17/cron-monitors')
          return Response.json({ items: [], total: 0, page: 1, page_size: 20 })
        return Response.json({ configured: true })
      },
      { preconnect: fetch.preconnect }
    ),
  })
}

/** Renders the cron list under a project bar set to `environment`. */
function renderCronList(environment: {
  id: number
  name: string
  slug: string
}) {
  const query = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
  const project = { id: 17, slug: 'synthetic', name: 'Synthetic' }
  const bar = {
    environments: [environment],
    isPending: false,
    selection: { environment, all: false, source: 'url' },
  } as unknown as ProjectEnvironmentContextValue
  return render(
    <QueryClientProvider client={query}>
      <MemoryRouter initialEntries={['/']}>
        <ProjectEnvironmentContext.Provider value={bar}>
          <CronMonitors project={project as ProjectResponse} />
        </ProjectEnvironmentContext.Provider>
      </MemoryRouter>
    </QueryClientProvider>
  )
}

test('the cron page lists the .temps.yaml jobs Temps runs, next to check-in monitors', async () => {
  const paths: string[] = []
  serveCronList(paths)
  const view = renderCronList({ id: 3, name: 'production', slug: 'production' })

  expect(view.getByRole('heading', { name: 'Scheduled by Temps' })).toBeTruthy()
  expect(view.getByRole('heading', { name: 'Check-in monitors' })).toBeTruthy()
  const job = await view.findByRole('link', { name: /\/api\/tick-3/ })
  expect(job.getAttribute('href')).toBe(
    '/projects/synthetic/settings/cron-jobs/3/9'
  )
  expect(
    view
      .getAllByRole('link', { name: 'Create monitor' })
      .map((link) => link.getAttribute('href'))
  ).toContain('/projects/synthetic/cron-jobs/new')
})

test('the jobs follow the environment chosen in the project bar', async () => {
  const paths: string[] = []
  serveCronList(paths)
  const view = renderCronList({ id: 5, name: 'staging', slug: 'staging' })

  await view.findByRole('link', { name: /\/api\/tick-5/ })
  expect(paths).toContain('/projects/17/environments/5/crons')
  expect(paths).not.toContain('/projects/17/environments/3/crons')
  // The bar picks the environment; the section has no picker of its own.
  expect(view.queryByLabelText('Environment')).toBeNull()
})
