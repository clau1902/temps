// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useMutation, useQueryClient } from '@tanstack/react-query'
import { AlertTriangle, LockKeyhole, Network } from 'lucide-react'
import { useState } from 'react'
import { toast } from 'sonner'

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { CopyButton } from '@/components/ui/copy-button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { ReadFailure } from '@/components/ui/read-failure'
import { Skeleton } from '@/components/ui/skeleton'
import { useClusterNetwork } from '@/hooks/useClusterNetwork'
import { useSensitiveActionVerification } from '@/hooks/useSensitiveActionVerification'
import { problemDetail } from '@/lib/api-problem'
import { buildMultiNodeSetupCommand } from '@/lib/cluster-network-command'
import {
  clusterNetworkQueryKey,
  updateClusterNetwork,
  validatePoolInput,
  type ClusterNetworkStatus,
  type UpdateClusterNetworkRequest,
} from '@/lib/cluster-network'
import { isForbiddenRead } from '@/lib/read-failure'

/**
 * Settings -> Worker Nodes -> Cluster network (ADR 056): the compute pool
 * every node takes its overlay subnet from, changeable here while no node
 * uses it, and the control plane's own overlay failure when there is one.
 */
export function ClusterNetworkCard() {
  const { data, isLoading, error, refetch, isFetching } = useClusterNetwork()

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Network className="h-5 w-5" />
          Cluster network
        </CardTitle>
        <CardDescription>
          Containers reach each other across nodes on this private address pool.
          Each node gets one subnet of it.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <Skeleton className="h-24 w-full" />
        ) : error || !data ? (
          <ReadFailure
            resource="Cluster network"
            error={error}
            onRetry={isForbiddenRead(error) ? undefined : () => void refetch()}
            retrying={isFetching}
          />
        ) : (
          <ClusterNetworkForm
            key={`${data.compute_pool_cidr}/${data.subnet_prefix_len}`}
            status={data}
          />
        )}
      </CardContent>
    </Card>
  )
}

function ClusterNetworkForm({ status }: { status: ClusterNetworkStatus }) {
  const queryClient = useQueryClient()
  const [poolCidr, setPoolCidr] = useState(status.compute_pool_cidr)
  const [nodePrefix, setNodePrefix] = useState(String(status.subnet_prefix_len))
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()

  const update = useMutation({
    mutationFn: updateClusterNetwork,
    onSuccess: (answer) => {
      queryClient.setQueryData(clusterNetworkQueryKey, answer.status)
      const moved = answer.reassigned
        .map((node) => `${node.node_name} ${node.from} → ${node.to}`)
        .join(', ')
      toast.success(`Cluster pool is now ${answer.status.compute_pool_cidr}`, {
        description: moved
          ? `Moved ${moved}; the nodes apply their new subnets on their next network poll.`
          : undefined,
      })
    },
    onError: (failure, variables: UpdateClusterNetworkRequest) => {
      if (handleSensitiveActionError(failure, () => update.mutate(variables)))
        return
    },
  })

  const unchanged =
    poolCidr.trim() === status.compute_pool_cidr &&
    nodePrefix.trim() === String(status.subnet_prefix_len)
  const inputError = unchanged ? null : validatePoolInput(poolCidr, nodePrefix)
  const locked = !status.can_change
  const setupFailure = status.control_plane.setup_failure

  return (
    <div className="space-y-4">
      {setupFailure && (
        <Alert variant="destructive">
          <AlertTriangle className="h-4 w-4" />
          <AlertTitle>
            Cross-node network unavailable on the control plane
          </AlertTitle>
          <AlertDescription className="break-words">
            {setupFailure}
          </AlertDescription>
        </Alert>
      )}

      <form
        className="space-y-4"
        onSubmit={(event) => {
          event.preventDefault()
          if (locked || unchanged || inputError) return
          update.mutate({
            compute_pool_cidr: poolCidr.trim(),
            subnet_prefix_len: Number(nodePrefix),
          })
        }}
      >
        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-2">
            <Label htmlFor="cluster-compute-pool">Pool CIDR</Label>
            <Input
              id="cluster-compute-pool"
              value={poolCidr}
              disabled={locked || update.isPending}
              onChange={(event) => setPoolCidr(event.target.value)}
              spellCheck={false}
              className="font-mono"
              aria-invalid={inputError !== null}
              aria-describedby="cluster-compute-pool-help"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="cluster-node-prefix">Per-node prefix</Label>
            <Input
              id="cluster-node-prefix"
              value={nodePrefix}
              disabled={locked || update.isPending}
              onChange={(event) => setNodePrefix(event.target.value)}
              inputMode="numeric"
              className="font-mono"
              aria-invalid={inputError !== null}
              aria-describedby="cluster-compute-pool-help"
            />
          </div>
        </div>

        <p
          id="cluster-compute-pool-help"
          className={
            inputError
              ? 'text-xs text-destructive'
              : 'text-xs text-muted-foreground'
          }
        >
          {inputError ??
            `Must be private address space, outside the WireGuard mesh pool (${status.mesh_pool_cidr}) and anything your nodes already use. New clusters start with ${status.default_compute_pool_cidr}.`}
        </p>

        {update.error && !update.isPending && (
          <Alert variant="destructive">
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>Pool not changed</AlertTitle>
            <AlertDescription className="break-words">
              {problemDetail(update.error, 'The server refused the change.')}
            </AlertDescription>
          </Alert>
        )}

        {locked ? (
          <div className="flex gap-2 rounded-md bg-muted/50 p-3 text-xs text-muted-foreground">
            <LockKeyhole className="h-4 w-4 shrink-0" />
            <div className="space-y-1">
              <p>
                The pool is in use, so it cannot change in place: moving a
                running cross-node network would re-address every container on
                it. In use by:
              </p>
              <ul className="list-disc pl-4">
                {status.change_blocked_by.map((reason) => (
                  <li key={reason} className="break-words">
                    {reason}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        ) : (
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-xs text-muted-foreground max-w-prose">
              No node uses the pool yet. Subnets handed out but never applied
              move to the new pool, and their nodes pick them up on their next
              network poll.
            </p>
            <Button
              type="submit"
              size="sm"
              disabled={unchanged || inputError !== null || update.isPending}
            >
              {update.isPending ? 'Saving…' : 'Change pool'}
            </Button>
          </div>
        )}
      </form>

      {!locked && !unchanged && inputError === null && (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <span>Or on the control-plane host:</span>
          <code className="min-w-0 truncate">
            {buildMultiNodeSetupCommand(poolCidr.trim(), nodePrefix.trim())}
          </code>
          <CopyButton
            minimal
            value={buildMultiNodeSetupCommand(
              poolCidr.trim(),
              nodePrefix.trim()
            )}
            label="Copy network setup command"
          />
        </div>
      )}
      {verificationDialog}
    </div>
  )
}
