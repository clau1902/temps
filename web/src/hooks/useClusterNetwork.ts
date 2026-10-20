// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useQuery } from '@tanstack/react-query'

import {
  clusterNetworkQueryKey,
  fetchClusterNetwork,
} from '@/lib/cluster-network'
import { refetchUnlessRefused } from '@/lib/query-retry'

/** The cluster network status (ADR 056), polled with the node list. */
export function useClusterNetwork() {
  return useQuery({
    queryKey: clusterNetworkQueryKey,
    queryFn: fetchClusterNetwork,
    // A refused read (a role without settings:read) is not asked again.
    refetchInterval: refetchUnlessRefused(30_000),
  })
}
