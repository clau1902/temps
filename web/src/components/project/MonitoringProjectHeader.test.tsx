// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, render } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import type { EnvironmentResponse, ProjectResponse } from '@/api/client'
import {
  ProjectEnvironmentContext,
  type ProjectEnvironmentContextValue,
} from '@/components/project-frame/project-environment-context'
import { installBrowserWindow } from '@/test/dom'
import { MonitoringProjectHeader } from './MonitoringProjectOverview'

installBrowserWindow()
afterEach(cleanup)

const production = {
  id: 3,
  name: 'production',
  slug: 'production',
  is_preview: false,
} as EnvironmentResponse

function renderHeader(environments: EnvironmentResponse[]) {
  const bar = {
    environments,
    scope: 'all',
    selection: { environment: environments[0], all: false, source: 'url' },
    select: () => {},
    defaultEnvironment: environments[0],
  } as unknown as ProjectEnvironmentContextValue
  return render(
    <MemoryRouter initialEntries={['/projects/synthetic/errors']}>
      <ProjectEnvironmentContext.Provider value={bar}>
        <MonitoringProjectHeader
          project={
            { id: 1, slug: 'synthetic', name: 'Synthetic' } as ProjectResponse
          }
        />
      </ProjectEnvironmentContext.Provider>
    </MemoryRouter>
  )
}

test('a monitoring-only project picks the environment its pages filter by', () => {
  const view = renderHeader([production])
  const nav = view.getByRole('navigation', { name: 'Project and environment' })
  expect(nav.textContent).toContain('Synthetic')
  expect(view.getByRole('combobox', { name: 'Environment' })).toBeTruthy()
  // Only the picker: deploying is not something this project does.
  expect(view.queryByRole('button', { name: /Deploy/ })).toBeNull()
})

test('with no environments there is nothing to pick', () => {
  const view = renderHeader([])
  expect(view.queryByRole('combobox', { name: 'Environment' })).toBeNull()
})
