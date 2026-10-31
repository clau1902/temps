// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, render } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import { resolveProjectLocation } from '@/lib/project-navigation'
import { installBrowserWindow } from '@/test/dom'
import { ProjectSectionTabs } from './ProjectSectionTabs'

installBrowserWindow()
afterEach(cleanup)

function renderTabs(route: string) {
  return render(
    <MemoryRouter initialEntries={[`/projects/synthetic-api/${route}`]}>
      <ProjectSectionTabs
        projectSlug="synthetic-api"
        location={resolveProjectLocation(route)}
      />
    </MemoryRouter>
  )
}

test('edge rules is a tab of Security & access, current on its page', () => {
  const view = renderTabs('settings/edge-rules')
  const nav = view.getByRole('navigation', { name: 'Security & access pages' })
  const tab = view.getByRole('link', { name: 'Edge rules' })
  expect(nav.contains(tab)).toBe(true)
  expect(tab.getAttribute('aria-current')).toBe('page')
  expect(tab.getAttribute('href')).toBe(
    '/projects/synthetic-api/settings/edge-rules'
  )
})

test('a rule opened for editing keeps the Edge rules tab current', () => {
  const view = renderTabs('settings/edge-rules/new')
  expect(
    view.getByRole('link', { name: 'Edge rules' }).getAttribute('aria-current')
  ).toBe('page')
})

test('tab icons sit before the label and drop on a phone', () => {
  const view = renderTabs('settings/security')
  const icon = view.getByRole('link', { name: 'Protection' }).firstElementChild
  expect(icon?.tagName.toLowerCase()).toBe('svg')
  expect(icon?.getAttribute('aria-hidden')).toBe('true')
  expect(icon?.getAttribute('class')).toContain('max-sm:hidden')
})
