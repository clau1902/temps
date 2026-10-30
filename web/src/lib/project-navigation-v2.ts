// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Navigation model for the v2 project layout (prototype).
 *
 * The v2 layout lives at `/projects/:slug/v2/*` and reuses the classic page
 * components under the same sub-paths, so a tab URL here is exactly the
 * classic sub-path (`deployments`, `errors/feedback`, …). What changes is the
 * grouping: one menu grouped by job instead of a flat list, with each
 * section's pages as underline tabs instead of a second navigation column.
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

export interface ProjectV2Tab {
  title: string
  /** Overrides the section's width for this tab. */
  width?: PageWidth
  /** Sub-path below the project root, shared with the classic layout. */
  url: string
  /** Other sub-path prefixes that belong to this tab (detail pages, legacy). */
  aliases?: string[]
}

export type ProjectV2SectionId =
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

export interface ProjectV2Section {
  id: ProjectV2SectionId
  title: string
  environmentScope: EnvironmentScope
  width: PageWidth
  /** The first tab is the section's landing page. */
  tabs: [ProjectV2Tab, ...ProjectV2Tab[]]
}

export interface ProjectV2Group {
  /** Omitted for the ungrouped first entry (Overview). */
  label?: string
  sections: ProjectV2Section[]
}

/** Sub-path of the v2 Build & deploy page; it has no classic equivalent. */
export const BUILD_AND_DEPLOY_PATH = 'build-deploy'

export const PROJECT_V2_GROUPS: ProjectV2Group[] = [
  {
    sections: [
      {
        id: 'overview',
        title: 'Overview',
        environmentScope: 'single',
        width: 'full',
        tabs: [
          { title: 'Overview', url: 'project' },
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

export const PROJECT_V2_SECTIONS: ProjectV2Section[] =
  PROJECT_V2_GROUPS.flatMap((group) => group.sections)

function matchesPrefix(route: string, prefix: string): boolean {
  return route === prefix || route.startsWith(`${prefix}/`)
}

export interface ProjectV2Location {
  section: ProjectV2Section
  tab: ProjectV2Tab
}

/**
 * The section and tab a sub-path belongs to. The longest matching prefix
 * wins, so `errors/alert-rules` resolves to the Alerts tab rather than Issues
 * and `metrics/dashboards/3` to Dashboards rather than Metrics. An unknown
 * sub-path falls back to the Overview so the menu always highlights something.
 */
export function resolveProjectV2Location(route: string): ProjectV2Location {
  const normalized = route.replace(/^\/+|\/+$/g, '')
  let best: (ProjectV2Location & { length: number }) | undefined
  for (const section of PROJECT_V2_SECTIONS) {
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
  const overview = PROJECT_V2_SECTIONS[0]
  return { section: overview, tab: overview.tabs[0] }
}

/**
 * Whether the section's tab strip already names the page at `route`: the
 * strip is shown (two or more tabs) and `route` is a tab's own page, not a
 * record below it (`errors`, not `errors/42`). The page then does not print
 * its title a second time.
 */
export function projectV2TabNamesPage(
  route: string,
  location: ProjectV2Location
): boolean {
  if (location.section.tabs.length < 2) return false
  const normalized = route.replace(/^\/+|\/+$/g, '')
  return [location.tab.url, ...(location.tab.aliases ?? [])].includes(
    normalized
  )
}

/** Root of the v2 layout for a project. */
export function projectV2Root(slug: string): string {
  return `/projects/${slug}/v2`
}

/**
 * The v2 equivalent of a classic project URL path, or `null` when the path is
 * not inside that project. Paths already in v2 are returned unchanged.
 */
export function toProjectV2Path(slug: string, pathname: string): string | null {
  const root = `/projects/${slug}`
  if (pathname !== root && !pathname.startsWith(`${root}/`)) return null
  const rest = pathname.slice(root.length).replace(/^\/+/, '')
  if (rest === 'v2' || rest.startsWith('v2/')) return pathname
  return rest ? `${root}/v2/${rest}` : `${root}/v2`
}

/**
 * The classic equivalent of a v2 project URL path. Build & deploy has no
 * classic page of its own and maps to the delivery settings it embeds.
 */
export function toClassicProjectPath(slug: string, pathname: string): string {
  const root = `/projects/${slug}`
  const v2Root = `${root}/v2`
  if (pathname !== v2Root && !pathname.startsWith(`${v2Root}/`)) return pathname
  const rest = pathname.slice(v2Root.length).replace(/^\/+/, '')
  if (matchesPrefix(rest, BUILD_AND_DEPLOY_PATH))
    return `${root}/settings/delivery`
  return rest ? `${root}/${rest}` : root
}

/** Query value of the environments page's settings view. */
const ENVIRONMENT_SETTINGS_VIEW = 'settings'

/**
 * The width a v2 page gets. The environment settings view is a form, though
 * the rest of the environments section is a dashboard.
 */
export function projectV2PageWidth(
  location: ProjectV2Location,
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
