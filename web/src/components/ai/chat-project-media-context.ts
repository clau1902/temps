// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { LatestDeploymentMediaResponseItem } from '@/api/client/types.gen'
import { createContext, useContext } from 'react'

export type MediaByProject = Record<string, LatestDeploymentMediaResponseItem>

/** The chat list's latest deployment media, keyed by project id. */
export const ChatProjectMediaContext = createContext<MediaByProject | null>(
  null
)

/** The latest deployment media of one listed project, once loaded. */
export function useChatProjectMedia(
  projectId: number
): LatestDeploymentMediaResponseItem | undefined {
  return useContext(ChatProjectMediaContext)?.[String(projectId)]
}
