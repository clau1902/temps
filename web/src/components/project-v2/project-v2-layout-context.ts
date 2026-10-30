// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { createContext, useContext } from 'react'

/**
 * True inside the v2 project layout. A classic page that brings its own
 * navigation (Metrics' Explore / Dashboards / Alerts strip) drops it there,
 * because the v2 section tabs already offer the same pages.
 */
export const ProjectV2LayoutContext = createContext(false)

export function useInProjectV2Layout(): boolean {
  return useContext(ProjectV2LayoutContext)
}
