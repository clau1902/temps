// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, fireEvent, render, within } from '@testing-library/react'
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

const liveProduction = {
  ...production,
  current_deployment_id: 40,
  sleeping: false,
  main_url: 'https://synthetic-api.example.test',
} as EnvironmentResponse

type Routes = Parameters<typeof mockApi>[0]

function renderBar(
  project: ProjectResponse,
  membership: ProjectGroupResponse | null = null,
  {
    environment = production,
    routes = {},
  }: { environment?: EnvironmentResponse; routes?: Routes } = {}
) {
  mockApi({
    'GET /projects/7/group': () => jsonResponse({ group: membership }),
    ...routes,
  })
  const bar = {
    environments: [environment],
    isPending: false,
    isError: false,
    retry: () => {},
    scope: 'single',
    selection: { environment, all: false, source: 'url' },
    select: () => {},
    defaultEnvironment: environment,
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

const deployments = (status: string) => () =>
  jsonResponse({ deployments: [{ id: 91, status }], total: 1 })

test('a failed latest deployment reads Failed and opens that deployment', async () => {
  const view = renderBar(service, null, {
    environment: liveProduction,
    routes: { 'GET /projects/7/deployments': deployments('failed') },
  })

  const failed = await view.findByRole('link', { name: /^Failed: / })
  expect(failed.getAttribute('href')).toBe(
    '/projects/synthetic-api/deployments/91'
  )
  expect(failed.getAttribute('aria-label')).toContain(
    'previous version is still serving'
  )
})

test('monitors that cannot reach production read Down and open Uptime', async () => {
  const view = renderBar(service, null, {
    environment: liveProduction,
    routes: {
      'GET /projects/7/deployments': deployments('completed'),
      'GET /monitors-health/projects': () =>
        jsonResponse({ projects: { '7': { project_id: 7, status: 'down' } } }),
    },
  })

  const down = await view.findByRole('link', { name: /^Down: / })
  expect(down.getAttribute('href')).toBe('/projects/synthetic-api/monitors')
})

test('a healthy environment reads Live, without a link', async () => {
  const view = renderBar(service, null, {
    environment: liveProduction,
    routes: { 'GET /projects/7/deployments': deployments('completed') },
  })

  expect(await view.findByRole('img', { name: /^Live: / })).toBeTruthy()
  expect(view.queryByRole('link', { name: /^Live/ })).toBeNull()
})

test('the repository opens from the bar, without credentials in the link', () => {
  const view = renderBar({
    ...service,
    git_url: 'https://token:secret@github.com/example/synthetic-api.git',
  } as ProjectResponse)

  expect(
    view
      .getByRole('link', { name: 'Open repository in a new window' })
      .getAttribute('href')
  ).toBe('https://github.com/example/synthetic-api')
})

test('a live environment says where visitors reach it', async () => {
  const view = renderBar(service, null, {
    environment: liveProduction,
    routes: {
      'GET /projects/7/environments/3/reachability': () =>
        jsonResponse({
          environment_id: 3,
          state: 'reachable',
          summary: 'Public on the internet.',
          primary_address: { address: 'synthetic-api.example.test' },
          addresses: [
            {
              address: 'synthetic-api.example.test',
              state: 'reachable',
              audience: 'public',
              source: 'environment_domain',
            },
          ],
        }),
    },
  })

  expect(await view.findByText('synthetic-api.example.test')).toBeTruthy()
})

test('an app hosted elsewhere reports no deployment state of its own', async () => {
  const view = renderBar(
    { ...service, hosting: 'external' } as ProjectResponse,
    null,
    {
      environment: liveProduction,
      routes: { 'GET /projects/7/deployments': deployments('failed') },
    }
  )

  await view.findByText('External', { exact: true })
  expect(view.queryByRole('link', { name: /^Failed/ })).toBeNull()
  expect(view.queryByRole('img', { name: /^Live/ })).toBeNull()
})

test('on a phone the actions are one menu', async () => {
  const view = renderBar(
    {
      ...service,
      git_url: 'https://github.com/example/synthetic-api.git',
    } as ProjectResponse,
    null,
    { environment: liveProduction }
  )

  const trigger = view.getByRole('button', { name: 'Project actions' })
  fireEvent.pointerDown(trigger, {
    button: 0,
    ctrlKey: false,
    pointerType: 'mouse',
  })
  const menu = await view.findByRole('menu')
  for (const name of ['Deploy', 'Visit production', 'Open repository'])
    expect(within(menu).getByRole('menuitem', { name })).toBeTruthy()
  expect(within(menu).getAllByRole('menuitem')).toHaveLength(3)
})
