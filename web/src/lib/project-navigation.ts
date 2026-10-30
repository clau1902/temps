// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Navigation model for a project (`/projects/:slug/*`): one menu grouped by
 * job (Ship, Observe, Configure), each section's pages shown as underline
 * tabs above the page. A tab's URL is the page's sub-path below the project
 * (`deployments`, `errors/feedback`, …); `aliases` are older or nested paths
 * that belong to the same tab.
 */

/** How the shared environment bar applies to a section's pages. */
export type EnvironmentScope =
  /** The page shows one environment; the bar picks which. */
  | 'single'
  /** The page lists records; the bar can also select "All environments". */
  | 'all'
  /** The page is project-wide; the bar is informational. */
  | 'project'

import type { PageWidth } from '@temps-sdk/ds'

/** How wide a page may grow; see `PageWidth` in the design system. */
export type { PageWidth }

export interface ProjectNavTab {
  title: string
  /** Overrides the section's width for this tab. */
  width?: PageWidth
  /** Sub-path below the project root (`/projects/:slug/<url>`). */
  url: string
  /** Other sub-path prefixes that belong to this tab (detail pages, legacy). */
  aliases?: string[]
}

export type ProjectSectionId =
  | 'overview'
  | 'deployments'
  | 'environments'
  | 'domains'
  | 'databases'
  | 'logs'
  | 'errors'
  | 'traces'
  | 'analytics'
  | 'monitoring'
  | 'build'
  | 'variables'
  | 'access'
  | 'automation'
  | 'general'

export interface ProjectNavSection {
  id: ProjectSectionId
  title: string
  environmentScope: EnvironmentScope
  width: PageWidth
  /** The first tab is the section's landing page. */
  tabs: [ProjectNavTab, ...ProjectNavTab[]]
}

export interface ProjectNavGroup {
  /** Omitted for the ungrouped first entry (Overview). */
  label?: string
  sections: ProjectNavSection[]
}

/** Sub-path of the Build & deploy page, which embeds the delivery settings. */
export const BUILD_AND_DEPLOY_PATH = 'build-deploy'

export const PROJECT_NAV_GROUPS: ProjectNavGroup[] = [
  {
    sections: [
      {
        id: 'overview',
        title: 'Overview',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          // A monitoring-only project's overview also answers `integrations`.
          { title: 'Overview', url: 'project', aliases: ['integrations'] },
          { title: 'Activity', url: 'observe' },
        ],
      },
    ],
  },
  {
    label: 'Ship',
    sections: [
      {
        id: 'deployments',
        title: 'Deployments',
        environmentScope: 'all',
        width: 'full',
        tabs: [{ title: 'Deployments', url: 'deployments', aliases: ['drop'] }],
      },
      {
        id: 'environments',
        title: 'Environments',
        environmentScope: 'single',
        width: 'full',
        tabs: [{ title: 'Environments', url: 'environments' }],
      },
      {
        id: 'domains',
        title: 'Domains',
        environmentScope: 'project',
        width: 'full',
        tabs: [
          { title: 'Domains', url: 'domains', aliases: ['settings/domains'] },
        ],
      },
      {
        id: 'databases',
        title: 'Databases',
        environmentScope: 'project',
        width: 'full',
        tabs: [
          { title: 'Databases', url: 'storage', aliases: ['databases'] },
          { title: 'Key-value store', url: 'services/kv' },
          { title: 'Blob storage', url: 'services/blob' },
        ],
      },
    ],
  },
  {
    label: 'Observe',
    sections: [
      {
        id: 'logs',
        title: 'Logs',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          { title: 'Application', url: 'runtime' },
          { title: 'Requests', url: 'request-logs', aliases: ['logs'] },
          { title: 'Telemetry', url: 'telemetry-logs' },
        ],
      },
      {
        id: 'errors',
        title: 'Errors',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          { title: 'Issues', url: 'errors' },
          { title: 'Alerts', url: 'errors/alert-rules' },
          { title: 'Feedback', url: 'errors/feedback' },
          { title: 'Releases', url: 'releases' },
          { title: 'Source maps', url: 'errors/source-maps' },
          { title: 'SDK health', url: 'client-outcomes' },
          { title: 'Setup', url: 'errors/setup' },
        ],
      },
      {
        id: 'traces',
        title: 'Traces',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          { title: 'Traces', url: 'traces' },
          { title: 'AI traces', url: 'ai-gateway' },
        ],
      },
      {
        id: 'analytics',
        title: 'Analytics',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          { title: 'Overview', url: 'analytics' },
          { title: 'Visitors', url: 'analytics/visitors' },
          { title: 'Live', url: 'analytics/live-visitors' },
          { title: 'Globe', url: 'analytics/live' },
          { title: 'Journey', url: 'analytics/journey' },
          { title: 'Sessions', url: 'analytics/replays', width: 'full' },
          { title: 'Pages', url: 'analytics/pages' },
          { title: 'Funnels', url: 'analytics/funnels' },
          { title: 'Activity report', url: 'analytics/activity' },
          { title: 'Web performance', url: 'speed' },
          { title: 'AI visitors', url: 'analytics/ai-agents' },
          { title: 'AI crawlers', url: 'ai-crawlers' },
          { title: 'API traffic', url: 'analytics/api-traffic' },
          { title: 'Revenue', url: 'revenue' },
        ],
      },
      {
        id: 'monitoring',
        title: 'Monitoring',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          // The Metrics page's own Explore / Dashboards / Alerts strip is
          // folded into these tabs, so there is one row of tabs, not two.
          { title: 'Metrics', url: 'metrics/explore', aliases: ['metrics'] },
          {
            title: 'Dashboards',
            url: 'metrics/dashboards',
            aliases: ['dashboards'],
          },
          { title: 'Alerts', url: 'metrics/alerts' },
          { title: 'Uptime', url: 'monitors' },
          {
            title: 'Cron jobs',
            url: 'cron-jobs',
            aliases: ['settings/cron-jobs'],
          },
        ],
      },
    ],
  },
  {
    label: 'Configure',
    sections: [
      {
        id: 'build',
        title: 'Build & deploy',
        environmentScope: 'single',
        width: 'form',
        tabs: [
          {
            title: 'Build & deploy',
            url: BUILD_AND_DEPLOY_PATH,
            aliases: [
              'settings/delivery',
              'settings/build',
              'settings/git',
              'build',
              'git',
              'connect-repository',
              'flags',
            ],
          },
        ],
      },
      {
        id: 'variables',
        title: 'Variables & secrets',
        environmentScope: 'project',
        // Tables of keys and values, not forms.
        width: 'full',
        tabs: [
          {
            title: 'Environment variables',
            url: 'environment-variables',
            aliases: ['settings/environment-variables'],
          },
          {
            title: 'Secrets',
            url: 'settings/variables',
            aliases: ['settings/secrets'],
          },
          { title: 'Deployment tokens', url: 'settings/deployment-tokens' },
        ],
      },
      {
        id: 'access',
        title: 'Security & access',
        environmentScope: 'project',
        width: 'form',
        tabs: [
          { title: 'Protection', url: 'settings/security' },
          { title: 'Access', url: 'settings/access' },
          { title: 'Scans', url: 'security', width: 'full' },
        ],
      },
      {
        id: 'automation',
        title: 'Automation & integrations',
        environmentScope: 'project',
        width: 'form',
        tabs: [
          {
            title: 'Automation',
            url: 'settings/automation',
            aliases: ['agents', 'autofixer'],
          },
          {
            title: 'Integrations',
            url: 'settings/integrations',
            aliases: [
              'settings/webhooks',
              'settings/skills',
              'settings/mcp-servers',
            ],
          },
          { title: 'Telemetry', url: 'settings/telemetry' },
        ],
      },
      {
        id: 'general',
        title: 'General',
        environmentScope: 'project',
        width: 'form',
        tabs: [
          {
            title: 'General',
            url: 'settings/general',
            aliases: ['settings', 'tools'],
          },
        ],
      },
    ],
  },
]

export const PROJECT_NAV_SECTIONS: ProjectNavSection[] =
  PROJECT_NAV_GROUPS.flatMap((group) => group.sections)

function matchesPrefix(route: string, prefix: string): boolean {
  return route === prefix || route.startsWith(`${prefix}/`)
}

export interface ProjectNavLocation {
  section: ProjectNavSection
  tab: ProjectNavTab
}

/**
 * The section and tab a sub-path belongs to. The longest matching prefix
 * wins, so `errors/alert-rules` resolves to the Alerts tab rather than Issues
 * and `metrics/dashboards/3` to Dashboards rather than Metrics. An unknown
 * sub-path falls back to the Overview so the menu always highlights something.
 */
export function resolveProjectLocation(route: string): ProjectNavLocation {
  const normalized = route.replace(/^\/+|\/+$/g, '')
  let best: (ProjectNavLocation & { length: number }) | undefined
  for (const section of PROJECT_NAV_SECTIONS) {
    for (const tab of section.tabs) {
      for (const prefix of [tab.url, ...(tab.aliases ?? [])]) {
        if (
          matchesPrefix(normalized, prefix) &&
          (!best || prefix.length > best.length)
        ) {
          best = { section, tab, length: prefix.length }
        }
      }
    }
  }
  if (best) return { section: best.section, tab: best.tab }
  const overview = PROJECT_NAV_SECTIONS[0]
  return { section: overview, tab: overview.tabs[0] }
}

/**
 * Whether the section's tab strip already names the page at `route`: the
 * strip is shown (two or more tabs) and `route` is a tab's own page, not a
 * record below it (`errors`, not `errors/42`). The page then does not print
 * its title a second time.
 */
export function projectTabNamesPage(
  route: string,
  location: ProjectNavLocation
): boolean {
  if (location.section.tabs.length < 2) return false
  const normalized = route.replace(/^\/+|\/+$/g, '')
  return [location.tab.url, ...(location.tab.aliases ?? [])].includes(
    normalized
  )
}

/** Root URL of a project's pages. */
export function projectRoot(slug: string): string {
  return `/projects/${slug}`
}

/** Query value of the environments page's settings view. */
const ENVIRONMENT_SETTINGS_VIEW = 'settings'

/**
 * The width a project page gets. The environment settings view is a form,
 * though the rest of the environments section is a dashboard.
 */
export function projectPageWidth(
  location: ProjectNavLocation,
  search: string
): PageWidth {
  if (location.tab.width) return location.tab.width
  if (
    location.section.id === 'environments' &&
    new URLSearchParams(search).get('view') === ENVIRONMENT_SETTINGS_VIEW
  )
    return 'form'
  return location.section.width
}

/**
 * Project-relative pages a monitoring-only (`source_type: 'external'`)
 * project registers — see the external route table in `ProjectDetail`.
 * Everything else is a hosting page (deployments, variables, delivery
 * settings, ...) that such a project answers with "Add hosting", so the
 * command palette does not offer it as a destination.
 */
const EXTERNAL_PROJECT_PAGES = [
  'project',
  'hosting',
  'analytics',
  'speed',
  'revenue',
  'errors',
  'traces',
  'ai-gateway',
  'telemetry-logs',
  'releases',
  'client-outcomes',
  'metrics',
  'monitors',
  'cron-jobs',
  'status-pages',
  'settings/general',
  'settings/telemetry',
] as const

/** Whether a project-relative URL is a page an external project has. */
export function isExternalProjectPage(url: string): boolean {
  const route = url.split(/[?#]/)[0]
  return EXTERNAL_PROJECT_PAGES.some((page) => matchesPrefix(route, page))
}

/** Whether `url` is a page `project` has, given its kind. */
export function projectHasPage(
  project: { source_type?: string | null },
  url: string
): boolean {
  return project.source_type !== 'external' || isExternalProjectPage(url)
}

/**
 * The menu `project` actually has: a monitoring-only project keeps only the
 * tabs it registers, and sections left without a tab disappear.
 */
export function projectNavGroupsFor(project: {
  source_type?: string | null
}): ProjectNavGroup[] {
  if (project.source_type !== 'external') return PROJECT_NAV_GROUPS
  return PROJECT_NAV_GROUPS.map((group) => ({
    ...group,
    sections: group.sections.flatMap((section): ProjectNavSection[] => {
      const tabs = section.tabs.filter((tab) =>
        projectHasPage(project, tab.url)
      )
      const [first, ...rest] = tabs
      return first ? [{ ...section, tabs: [first, ...rest] }] : []
    }),
  })).filter((group) => group.sections.length > 0)
}

/**
 * Where `route` sits in `project`'s own menu: like `resolveProjectLocation`,
 * with the section's tabs narrowed to the pages the project has.
 */
export function projectLocationFor(
  project: { source_type?: string | null },
  route: string
): ProjectNavLocation {
  const location = resolveProjectLocation(route)
  const [first, ...rest] = location.section.tabs.filter((tab) =>
    projectHasPage(project, tab.url)
  )
  return first
    ? { ...location, section: { ...location.section, tabs: [first, ...rest] } }
    : location
}
