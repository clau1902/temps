// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, render } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import type {
  EnvironmentResponse,
  ProjectGroupResponse,
  ProjectResponse,
} from '@/api/client'
import { installBrowserWindow } from '@/test/dom'
import { jsonResponse, mockApi } from '@/test/fetch-mock'
import { ProjectEnvironmentBar } from './ProjectEnvironmentBar'
import {
  ProjectEnvironmentContext,
  type ProjectEnvironmentContextValue,
} from './project-environment-context'

installBrowserWindow()
afterEach(cleanup)

const production = {
  id: 3,
  name: 'production',
  slug: 'production',
  is_preview: false,
} as EnvironmentResponse

const service = {
  id: 7,
  slug: 'synthetic-api',
  name: 'synthetic-api',
  hosting: 'hosted',
  source_type: 'manual',
} as ProjectResponse

const group = {
  id: 2,
  name: 'Synthetic shop',
  slug: 'synthetic-shop',
  projects: [{ id: 7, name: 'synthetic-api', slug: 'synthetic-api' }],
} as unknown as ProjectGroupResponse

function renderBar(
  project: ProjectResponse,
  membership: ProjectGroupResponse | null = null
) {
  mockApi({
    'GET /projects/7/group': () => jsonResponse({ group: membership }),
  })
  const bar = {
    environments: [production],
    isPending: false,
    isError: false,
    retry: () => {},
    scope: 'single',
    selection: { environment: production, all: false, source: 'url' },
    select: () => {},
    defaultEnvironment: production,
    setDefault: () => {},
  } as unknown as ProjectEnvironmentContextValue
  return render(
    <QueryClientProvider
      client={
        new QueryClient({ defaultOptions: { queries: { retry: false } } })
      }
    >
      <MemoryRouter initialEntries={['/projects/synthetic-api/project']}>
        <ProjectEnvironmentContext.Provider value={bar}>
          <ProjectEnvironmentBar project={project} onDeploy={() => {}} />
        </ProjectEnvironmentContext.Provider>
      </MemoryRouter>
    </QueryClientProvider>
  )
}

test('the project name is the page heading, and a standalone service can join a project', async () => {
  const view = renderBar(service)

  expect(
    view.getByRole('heading', { level: 1, name: 'synthetic-api' })
  ).toBeTruthy()
  expect(
    await view.findByRole('button', { name: 'Add to project' })
  ).toBeTruthy()
  expect(view.getByRole('button', { name: 'Deploy' })).toBeTruthy()
  expect(view.queryByText('External', { exact: true })).toBeNull()
})

test('a grouped service names its project and switches between its services', async () => {
  const view = renderBar(service, group)

  expect(
    await view.findByRole('link', { name: 'Project Synthetic shop' })
  ).toBeTruthy()
  expect(
    view.getByRole('button', {
      name: 'synthetic-api: switch to another service in Synthetic shop',
    })
  ).toBeTruthy()
  expect(view.queryByRole('button', { name: 'Add to project' })).toBeNull()
})

test('an app hosted elsewhere is marked External and offers no deploy', () => {
  const view = renderBar({ ...service, hosting: 'external' } as ProjectResponse)

  expect(view.getByText('External', { exact: true })).toBeTruthy()
  expect(view.queryByRole('button', { name: 'Deploy' })).toBeNull()
})

test('host Docker access is stated wherever the project is open', () => {
  const view = renderBar({
    ...service,
    docker_socket: { granted: true, nodes: [] },
  } as unknown as ProjectResponse)

  expect(
    view.getByRole('img', {
      name: 'Host Docker access: Granted on this installation.',
    })
  ).toBeTruthy()
})

test('without the grant there is no host-access badge', () => {
  const view = renderBar({
    ...service,
    docker_socket: { granted: false, nodes: [] },
  } as unknown as ProjectResponse)

  expect(view.queryByRole('img', { name: /Host Docker access/ })).toBeNull()
})
