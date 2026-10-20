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
