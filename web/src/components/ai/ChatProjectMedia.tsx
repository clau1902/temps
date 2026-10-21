// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { getLatestDeploymentMediaOptions } from '@/api/client/@tanstack/react-query.gen'
import type {
  LatestDeploymentMediaResponse,
  ProjectResponse,
} from '@/api/client/types.gen'
import { ROUTE_FAILURE_IGNORE } from '@/lib/route-query-failures'
import { useQueries } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import {
  chatProjectIds,
  chatProjectQueryOptions,
  mediaBatches,
  mediaProjectIds,
} from './chat-project-media'
import {
  ChatProjectMediaContext,
  type MediaByProject,
} from './chat-project-media-context'

const projectsOf = (results: ReadonlyArray<{ data?: ProjectResponse }>) =>
  results.map((result) => result.data)

const mergeMedia = (
  results: ReadonlyArray<{ data?: LatestDeploymentMediaResponse }>
): MediaByProject =>
  Object.assign({}, ...results.map((result) => result.data?.projects ?? {}))

/**
 * Loads the latest deployment media of every project in the chat list in one
 * polled request (per 100 projects), for the logos below it to read.
 */
export function ChatProjectMediaProvider({
  projectIds,
  children,
}: {
  projectIds: ReadonlyArray<number | null | undefined>
  children: ReactNode
}) {
  const projects = useQueries({
    queries: chatProjectIds(projectIds).map(chatProjectQueryOptions),
    combine: projectsOf,
  })
  const media = useQueries({
    queries: mediaBatches(mediaProjectIds(projects)).map((batch) => ({
      ...getLatestDeploymentMediaOptions({
        query: { project_ids: batch.join(',') },
      }),
      staleTime: 30_000,
      refetchInterval: 30_000,
      meta: { routeFailure: ROUTE_FAILURE_IGNORE },
    })),
    combine: mergeMedia,
  })
  return (
    <ChatProjectMediaContext.Provider value={media}>
      {children}
    </ChatProjectMediaContext.Provider>
  )
}
