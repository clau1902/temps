// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useLocation } from 'react-router'
import {
  type PageWidth,
  projectV2PageWidth,
  projectV2Root,
  projectV2TabNamesPage,
  resolveProjectV2Location,
} from '@/lib/project-navigation-v2'

/** The current URL below the project's v2 root, e.g. `errors/42`. */
function useProjectV2Route(slug: string): string {
  const { pathname } = useLocation()
  const root = projectV2Root(slug)
  return pathname.startsWith(root) ? pathname.slice(root.length) : ''
}

/** The v2 section and tab the current URL belongs to. */
export function useProjectV2Location(slug: string) {
  return resolveProjectV2Location(useProjectV2Route(slug))
}

/** Whether the section's tab strip already shows the current page's name. */
export function useProjectV2TabNamesPage(slug: string): boolean {
  const route = useProjectV2Route(slug)
  return projectV2TabNamesPage(route, resolveProjectV2Location(route))
}

/** How wide the current v2 page may grow (form, content or full). */
export function useProjectV2PageWidth(slug: string): PageWidth {
  const location = useProjectV2Location(slug)
  const { search } = useLocation()
  return projectV2PageWidth(location, search)
}
