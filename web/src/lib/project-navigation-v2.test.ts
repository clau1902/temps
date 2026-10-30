// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'

import {
  PROJECT_V2_GROUPS,
  PROJECT_V2_SECTIONS,
  projectV2PageWidth,
  projectV2TabNamesPage,
  resolveProjectV2Location,
  toClassicProjectPath,
  toProjectV2Path,
} from './project-navigation-v2'

describe('resolveProjectV2Location', () => {
  test('maps each section landing page to that section', () => {
    for (const section of PROJECT_V2_SECTIONS) {
      expect(resolveProjectV2Location(section.tabs[0].url).section.id).toBe(
        section.id
      )
    }
  })

  test('prefers the longest prefix across sections', () => {
    expect(resolveProjectV2Location('errors/alert-rules/new').tab.title).toBe(
      'Alerts'
    )
    expect(resolveProjectV2Location('metrics/dashboards/3').tab.title).toBe(
      'Dashboards'
    )
    expect(resolveProjectV2Location('metrics').tab.title).toBe('Metrics')
    expect(resolveProjectV2Location('metrics/alerts/new').tab.title).toBe(
      'Alerts'
    )
    const feedback = resolveProjectV2Location('errors/feedback/12')
    expect(feedback.section.id).toBe('errors')
    expect(feedback.tab.title).toBe('Feedback')
    expect(resolveProjectV2Location('errors/42').tab.title).toBe('Issues')
  })

  test('keeps detail pages inside their tab', () => {
    expect(resolveProjectV2Location('deployments/9').section.id).toBe(
      'deployments'
    )
    expect(
      resolveProjectV2Location('environment-variables/3/checks').section.id
    ).toBe('variables')
    expect(resolveProjectV2Location('security/scans/1').tab.title).toBe('Scans')
  })

  test('folds classic build, git and delivery settings into Build & deploy', () => {
    for (const route of [
      'settings/delivery',
      'connect-repository/connections/1',
    ]) {
      expect(resolveProjectV2Location(route).section.id).toBe('build')
    }
  })

  test('does not match a sibling that only shares a string prefix', () => {
    expect(resolveProjectV2Location('settings/securityx').section.id).toBe(
      'general'
    )
  })

  test('falls back to the overview for an unknown path', () => {
    expect(resolveProjectV2Location('nope').section.id).toBe('overview')
    expect(resolveProjectV2Location('').section.id).toBe('overview')
  })
})

describe('v2 and classic paths', () => {
  test('converts classic paths into v2 and leaves v2 paths alone', () => {
    expect(toProjectV2Path('app', '/projects/app/deployments/4')).toBe(
      '/projects/app/v2/deployments/4'
    )
    expect(toProjectV2Path('app', '/projects/app')).toBe('/projects/app/v2')
    expect(toProjectV2Path('app', '/projects/app/v2/errors')).toBe(
      '/projects/app/v2/errors'
    )
    expect(toProjectV2Path('app', '/projects/application/errors')).toBeNull()
  })

  test('converts v2 paths back, mapping Build & deploy to delivery settings', () => {
    expect(toClassicProjectPath('app', '/projects/app/v2/errors/3')).toBe(
      '/projects/app/errors/3'
    )
    expect(toClassicProjectPath('app', '/projects/app/v2/build-deploy')).toBe(
      '/projects/app/settings/delivery'
    )
    expect(toClassicProjectPath('app', '/projects/app/v2')).toBe(
      '/projects/app'
    )
  })
})

describe('projectV2TabNamesPage', () => {
  const names = (route: string) =>
    projectV2TabNamesPage(route, resolveProjectV2Location(route))

  test("a tab's own page is named by the tab strip", () => {
    expect(names('metrics/explore')).toBe(true)
    expect(names('metrics')).toBe(true)
    expect(names('errors')).toBe(true)
    expect(names('analytics/visitors')).toBe(true)
  })

  test('a record below a tab keeps its own title', () => {
    expect(names('errors/42')).toBe(false)
    expect(names('metrics/dashboards/3')).toBe(false)
    expect(names('security/scans/1')).toBe(false)
  })

  test('a section without a tab strip keeps its title', () => {
    expect(names('deployments')).toBe(false)
    expect(names('domains')).toBe(false)
    expect(names('settings/general')).toBe(false)
  })
})

describe('projectV2PageWidth', () => {
  const widthOf = (route: string, search = '') =>
    projectV2PageWidth(resolveProjectV2Location(route), search)

  test('Configure forms are narrow, its tables use the full width', () => {
    const configure = PROJECT_V2_GROUPS.find((g) => g.label === 'Configure')
    expect(configure).toBeDefined()
    const tables = new Set([
      'environment-variables',
      'settings/variables',
      'settings/deployment-tokens',
      'security',
    ])
    for (const section of configure?.sections ?? []) {
      for (const tab of section.tabs) {
        expect([tab.url, widthOf(tab.url)]).toEqual([
          tab.url,
          tables.has(tab.url) ? 'full' : 'form',
        ])
      }
    }
  })

  test('logs, traces and session replays use the full width', () => {
    expect(widthOf('runtime')).toBe('full')
    expect(widthOf('request-logs')).toBe('full')
    expect(widthOf('traces')).toBe('full')
    expect(widthOf('analytics/replays')).toBe('full')
    expect(widthOf('analytics/replays/abc')).toBe('full')
  })

  test('lists and dashboards use the full width', () => {
    expect(widthOf('deployments')).toBe('full')
    expect(widthOf('project')).toBe('full')
    expect(widthOf('analytics')).toBe('full')
    expect(widthOf('errors')).toBe('full')
  })

  test("an environment's settings view is a form, its dashboard is not", () => {
    expect(widthOf('environments', '?view=settings&env=production')).toBe(
      'form'
    )
    expect(widthOf('environments', '?view=containers')).toBe('full')
    expect(widthOf('environments')).toBe('full')
  })
})
