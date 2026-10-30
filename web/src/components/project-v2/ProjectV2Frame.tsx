// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { PageNamedByChromeContext } from '@temps-sdk/ds'
import type { ReactNode } from 'react'
import type { ProjectResponse } from '@/api/client'
import {
  useProjectV2Location,
  useProjectV2TabNamesPage,
} from './use-project-v2-location'
import { ProjectEnvironmentProvider } from './ProjectEnvironmentProvider'
import { ProjectV2LayoutContext } from './project-v2-layout-context'
import { ProjectSectionTabs } from './ProjectSectionTabs'

/** Shares the environment chosen in the bar with every v2 project page. */
export function ProjectV2Frame({
  project,
  children,
}: {
  project: ProjectResponse
  children: ReactNode
}) {
  const location = useProjectV2Location(project.slug)
  return (
    <ProjectV2LayoutContext.Provider value={true}>
      <ProjectEnvironmentProvider
        project={project}
        scope={location.section.environmentScope}
      >
        {children}
      </ProjectEnvironmentProvider>
    </ProjectV2LayoutContext.Provider>
  )
}

/**
 * The current section's tabs above its page. When the selected tab already
 * names the page, the page's own title is kept for screen readers only.
 */
export function ProjectV2Section({
  project,
  children,
}: {
  project: ProjectResponse
  children: ReactNode
}) {
  const location = useProjectV2Location(project.slug)
  const tabNamesPage = useProjectV2TabNamesPage(project.slug)
  return (
    <>
      <ProjectSectionTabs projectSlug={project.slug} location={location} />
      <PageNamedByChromeContext.Provider value={tabNamesPage}>
        {children}
      </PageNamedByChromeContext.Provider>
    </>
  )
}
