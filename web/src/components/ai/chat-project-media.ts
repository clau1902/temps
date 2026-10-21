// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Which projects the chat list asks the latest-deployment-media endpoint
 * about, and in how many requests. The list polls that media every 30
 * seconds, so it goes out as one batched request for every listed project
 * instead of one per project.
 */

import { getProjectOptions } from '@/api/client/@tanstack/react-query.gen'
import { ROUTE_FAILURE_IGNORE } from '@/lib/route-query-failures'

/** The most project ids the endpoint accepts in one request. */
export const MEDIA_BATCH_SIZE = 100

/** The distinct project ids of the chats, in a stable (sorted) order. */
export function chatProjectIds(
  projectIds: ReadonlyArray<number | null | undefined>
): number[] {
  const ids = new Set<number>()
  for (const id of projectIds) {
    if (typeof id === 'number' && Number.isFinite(id)) ids.add(id)
  }
  return [...ids].sort((a, b) => a - b)
}

/**
 * The projects worth asking about: ones the user could read (the project
 * loaded) that Temps deploys. An external project has no deployment media, and
 * a project the user lost access to would fail the whole batched request.
 */
export function mediaProjectIds(
  projects: ReadonlyArray<
    { id: number; source_type?: string | null } | null | undefined
  >
): number[] {
  return chatProjectIds(
    projects.map((project) =>
      project && project.source_type !== 'external' ? project.id : null
    )
  )
}

/** The ids split into requests the endpoint accepts. */
export function mediaBatches(
  ids: readonly number[],
  size: number = MEDIA_BATCH_SIZE
): number[][] {
  const batches: number[][] = []
  for (let start = 0; start < ids.length; start += size) {
    batches.push(ids.slice(start, start + size))
  }
  return batches
}

/**
 * A chat's project, for its logo. The list and each row share this query (same
 * key) and it never polls; logos are background context, so a failed read is
 * not reported as the page failing.
 */
export function chatProjectQueryOptions(projectId: number) {
  return {
    ...getProjectOptions({ path: { id: projectId } }),
    retry: false,
    staleTime: 60_000,
    meta: { routeFailure: ROUTE_FAILURE_IGNORE },
  }
}
