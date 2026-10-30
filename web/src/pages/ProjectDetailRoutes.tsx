// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect } from 'react'
import { Navigate, useLocation, useParams } from 'react-router'
import { toProjectV2Path } from '@/lib/project-navigation-v2'
import {
  prefersProjectV2,
  setPrefersProjectV2,
} from '@/lib/project-layout-preference'
import { ProjectDetail } from './ProjectDetail'

/**
 * Classic project URLs. Once the browser opted into the v2 prototype, links
 * the existing pages build (`/projects/:slug/deployments/12`) open in v2.
 */
export function ProjectDetailClassicRoute() {
  const { slug = '' } = useParams()
  const location = useLocation()
  const v2Path = prefersProjectV2()
    ? toProjectV2Path(slug, location.pathname)
    : null
  if (v2Path && v2Path !== location.pathname)
    return (
      <Navigate replace to={`${v2Path}${location.search}${location.hash}`} />
    )
  return <ProjectDetail />
}

/** `/projects/:slug/v2/*`: the prototype layout. Visiting it opts in. */
export function ProjectDetailV2Route() {
  useEffect(() => {
    setPrefersProjectV2(true)
  }, [])
  return <ProjectDetail layout="v2" />
}
