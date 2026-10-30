// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, within } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import type { EnvironmentResponse, ProjectResponse } from '@/api/client'
import { installBrowserWindow } from '@/test/dom'
import { jsonResponse, mockApi } from '@/test/fetch-mock'
import { EnvironmentShieldView } from './EnvironmentShieldView'

installBrowserWindow()
afterEach(cleanup)

const SHIELD = '/projects/3/environments/7/shield'

const SUMMARY = {
  project_id: 3,
  environment_id: 7,
  mode: 'log',
  mode_source: 'project',
  project_mode: 'log',
  window: '24h',
  start: '2026-10-29T10:00:00Z',
  end: '2026-10-30T10:00:00Z',
  storage: 'local',
  verdicts: { block: 4, challenge: 2, log: 597 },
  challenges: { issued: 0, solved: 0, failed: 0 },
  solve_times: { samples: 0, median_ms: null, p90_ms: null },
  sources: [{ source: 'synthetic-list', requests: 597 }],
  would_have: { blocked: 4, challenged: 2 },
}

function renderView() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <EnvironmentShieldView
          project={{ id: 3, slug: 'synthetic' } as ProjectResponse}
          environment={{ id: 7, name: 'staging' } as EnvironmentResponse}
          onOpenSettings={() => {}}
        />
      </MemoryRouter>
    </QueryClientProvider>
  )
}

test('the tab reads in three levels: mode, then Activity, then Tools', async () => {
  const api = mockApi({
    [`GET ${SHIELD}/summary`]: () => jsonResponse(SUMMARY),
    [`GET ${SHIELD}/events`]: () => jsonResponse({ events: [] }),
    [`GET ${SHIELD}/tokens`]: () => jsonResponse([]),
  })
  const view = renderView()

  const mode = await view.findByRole('heading', { level: 2, name: /^Mode: / })
  const groups = view
    .getAllByRole('heading', { level: 2 })
    .map((heading) => heading.textContent)
  expect(groups).toEqual([mode.textContent, 'Activity', 'Tools'])

  // Each section sits under its group, one level down.
  const sectionsOf = (group: string) => {
    const section = view
      .getByRole('heading', { level: 2, name: group })
      .closest('section')!
    // Empty states ("No Shield events yet") are headings too; skip them.
    return within(section)
      .getAllByRole('heading', { level: 3 })
      .map((heading) => heading.textContent)
      .filter((title) => !title?.startsWith('No '))
  }
  expect(sectionsOf('Activity')).toEqual([
    'Verdicts',
    'Challenges',
    'Recent events',
    'Requests by source',
  ])
  expect(sectionsOf('Tools')).toEqual([
    'Address lookup',
    'Bypass tokens',
    'Entries',
  ])

  // Logging only is a deliberate choice: what enforcing would have done is a
  // plain line, not an alert.
  expect(view.queryByRole('alert')).toBeNull()
  view.getByText(/Selective mode would have blocked/)
  expect(await view.findByText('synthetic-list')).toBeTruthy()
  api.restore()
})
