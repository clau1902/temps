// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  Key,
  Loader2,
  ShieldCheck,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  adminListNodesOptions,
  mintEnrollmentTokenMutation,
  wireguardMeshEnableMutation,
  wireguardMeshStatusGetOptions,
} from '@/api/client/@tanstack/react-query.gen'
import type { WireguardMeshStatusResponse } from '@/api/client/types.gen'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { CopyButton } from '@/components/ui/copy-button'
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from '@/components/ui/collapsible'
import { Skeleton } from '@/components/ui/skeleton'
import { TimeAgo } from '@/components/utils/TimeAgo'
import { QueryErrorAlert } from '@/components/nodes/mesh-ui'
import { ClusterNetworkForm } from '@/components/settings/ClusterNetworkCard'
import { ReadFailure } from '@/components/ui/read-failure'
import { useClusterNetwork } from '@/hooks/useClusterNetwork'
import {
  clusterNetworkQueryKey,
  updateClusterNetwork,
  type ClusterNetworkStatus,
  type UpdateClusterNetworkRequest,
} from '@/lib/cluster-network'
import { isForbiddenRead } from '@/lib/read-failure'
import { useWireguardMesh } from '@/components/nodes/WorkerJoinGuide'
import { useSensitiveActionVerification } from '@/hooks/useSensitiveActionVerification'
import { problemDetail } from '@/lib/api-problem'
import { cn } from '@/lib/utils'
import {
  controlPlaneMeshPort,
  joinUrl,
  offersOutboundJoin,
  outboundJoinCommand,
} from '@/lib/wireguard-mesh'

/** A join command's token registers one worker and lasts an hour. */
const ENROLLMENT_TOKEN_TTL_SECS = 3600
/** How often the node list is read while a worker is expected. */
const WAITING_POLL_MS = 5_000

/**
 * The quickest way to add a worker: turn on the private network, then run
 * one command on the new machine. The worker dials this server (ADR 055), so
 * it needs no public IP and no open port. Every other way to add a node is
 * under `otherWays`.
 */
export function AddWorkerNode({ otherWays }: { otherWays: ReactNode }) {
  const {
    data: mesh,
    isLoading,
    error,
    refetch,
    isFetching,
  } = useWireguardMesh()
  const network = useClusterNetwork()
  const { url } = joinUrl(mesh, window.location.origin)
  // A server that predates the confirmation, or a pool this role cannot
  // read, does not hold the steps back.
  const poolConfirmed =
    network.error || !network.data
      ? !network.isLoading
      : (network.data.pool_confirmed ?? true)
  const meshReady = mesh?.state === 'ready'

  return (
    <div className="space-y-4 rounded-lg border bg-muted/30 p-4">
      <div>
        <p className="text-sm font-medium">Add a worker node</p>
        <p className="text-sm text-muted-foreground">
          Any Linux machine, on any provider or at home. It connects to this
          server over an encrypted private network and needs no public IP.
        </p>
      </div>

      {isLoading ? (
        <div className="space-y-2">
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-16 w-full" />
        </div>
      ) : error || !mesh ? (
        <QueryErrorAlert
          title="Could not read the private network state"
          error={error}
          onRetry={() => void refetch()}
          retrying={isFetching}
        />
      ) : (
        <ol className="space-y-3">
          <WizardStep
            number={1}
            done={poolConfirmed}
            title="Container address range"
          >
            <PoolStep
              status={network.data}
              isLoading={network.isLoading}
              error={network.error}
              onRetry={() => void network.refetch()}
              retrying={network.isFetching}
            />
          </WizardStep>
          <WizardStep
            number={2}
            done={meshReady}
            disabled={!poolConfirmed}
            title="Private network"
          >
            {poolConfirmed && <PrivateNetworkStep mesh={mesh} />}
          </WizardStep>
          <WizardStep
            number={3}
            done={false}
            disabled={!poolConfirmed || !meshReady}
            title="Connect the machine"
          >
            {poolConfirmed &&
              meshReady &&
              (offersOutboundJoin(mesh, url) ? (
                <JoinCommandStep url={url} />
              ) : (
                <p className="text-sm text-muted-foreground">
                  This server is not reachable from the internet over HTTPS at a
                  public mesh endpoint, so a worker cannot dial it. Use one of
                  the other ways below.
                </p>
              ))}
          </WizardStep>
        </ol>
      )}

      <Collapsible>
        <CollapsibleTrigger className="group inline-flex items-center gap-1 text-sm font-medium text-muted-foreground hover:text-foreground">
          <ChevronDown className="h-4 w-4 transition-transform group-data-[state=open]:rotate-180" />
          Other ways to add a node
        </CollapsibleTrigger>
        <CollapsibleContent className="mt-3">{otherWays}</CollapsibleContent>
      </Collapsible>
    </div>
  )
}

function WizardStep({
  number,
  title,
  done,
  disabled = false,
  children,
}: {
  number: number
  title: string
  done: boolean
  disabled?: boolean
  children: ReactNode
}) {
  return (
    <li
      className={cn(
        'flex gap-3 rounded-md border bg-background p-3',
        disabled && 'opacity-60'
      )}
    >
      <span
        className={cn(
          'flex h-6 w-6 shrink-0 items-center justify-center rounded-full border text-xs font-medium',
          done && 'border-green-600 bg-green-600 text-white'
        )}
      >
        {done ? <CheckCircle2 className="h-4 w-4" /> : number}
      </span>
      <div className="min-w-0 flex-1 space-y-2">
        <p className="text-sm font-medium">{title}</p>
        {children}
      </div>
    </li>
  )
}

/**
 * The pool every node's containers take addresses from (ADR 056). It is
 * confirmed once, before the first node: after a node uses it, it can no
 * longer change in place. Saving it, changed or not, is the confirmation.
 */
function PoolStep({
  status,
  isLoading,
  error,
  onRetry,
  retrying,
}: {
  status: ClusterNetworkStatus | undefined
  isLoading: boolean
  error: unknown
  onRetry: () => void
  retrying: boolean
}) {
  const queryClient = useQueryClient()
  const [editing, setEditing] = useState(false)
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()
  const confirm = useMutation({
    mutationFn: updateClusterNetwork,
    onSuccess: (answer) => {
      queryClient.setQueryData(clusterNetworkQueryKey, answer.status)
    },
    onError: (failure, variables: UpdateClusterNetworkRequest) => {
      if (handleSensitiveActionError(failure, () => confirm.mutate(variables)))
        return
      toast.error('Could not confirm the address range', {
        description: problemDetail(failure, 'Try again.'),
      })
    },
  })

  if (isLoading) return <Skeleton className="h-10 w-full" />
  if (error || !status) {
    return (
      <ReadFailure
        resource="Cluster network"
        error={error}
        onRetry={isForbiddenRead(error) ? undefined : onRetry}
        retrying={retrying}
      />
    )
  }

  const range = <code className="font-mono">{status.compute_pool_cidr}</code>
  const perNode = `a /${status.subnet_prefix_len} per node`

  if (editing) {
    return (
      <div className="space-y-2">
        <ClusterNetworkForm status={status} onSaved={() => setEditing(false)} />
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => setEditing(false)}
        >
          Cancel
        </Button>
      </div>
    )
  }

  if (status.pool_confirmed ?? true) {
    return (
      <p className="text-sm text-muted-foreground">
        Containers get addresses from {range}, {perNode}.{' '}
        {status.can_change ? (
          <button
            type="button"
            className="font-medium underline"
            onClick={() => setEditing(true)}
          >
            Change
          </button>
        ) : (
          <>Nodes use it, so it can no longer change.</>
        )}
      </p>
    )
  }

  return (
    <div className="space-y-2">
      <p className="text-sm text-muted-foreground">
        Containers on every node get private addresses from {range}, {perNode}.
        Pick another range if your servers or VPN already use it: once a node
        joins, it cannot change.
      </p>
      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          disabled={confirm.isPending}
          onClick={() =>
            confirm.mutate({
              compute_pool_cidr: status.compute_pool_cidr,
              subnet_prefix_len: status.subnet_prefix_len,
            })
          }
        >
          {confirm.isPending && (
            <Loader2 className="mr-1 h-4 w-4 animate-spin" />
          )}
          Use {status.compute_pool_cidr}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={confirm.isPending}
          onClick={() => setEditing(true)}
        >
          Choose another range
        </Button>
      </div>
      {verificationDialog}
    </div>
  )
}

function PrivateNetworkStep({ mesh }: { mesh: WireguardMeshStatusResponse }) {
  const queryClient = useQueryClient()
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()
  const enable = useMutation({
    ...wireguardMeshEnableMutation(),
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: wireguardMeshStatusGetOptions().queryKey,
      })
    },
    onError: (error, variables) => {
      if (handleSensitiveActionError(error, () => enable.mutate(variables)))
        return
      toast.error('Could not turn on the private network', {
        description: problemDetail(error, 'Try again.'),
      })
    },
  })

  if (mesh.state === 'ready') {
    return (
      <p className="text-sm text-muted-foreground">
        On. This server is {mesh.control_plane?.address ?? 'on the mesh'}
        {mesh.cidr && <> on {mesh.cidr}</>}, and workers reach it at UDP{' '}
        <code>{controlPlaneMeshPort(mesh)}</code>.
      </p>
    )
  }

  if (mesh.state === 'starting' || enable.isPending) {
    return (
      <p className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        Starting the private network…
      </p>
    )
  }

  if (mesh.enable_blocker) {
    return (
      <Alert className="border-amber-500/30 bg-amber-500/5">
        <AlertTriangle className="h-4 w-4 text-amber-500" />
        <AlertTitle className="text-amber-700 dark:text-amber-400">
          This server cannot run the private network yet
        </AlertTitle>
        <AlertDescription className="text-amber-700 dark:text-amber-300">
          {mesh.enable_blocker}
        </AlertDescription>
      </Alert>
    )
  }

  return (
    <div className="space-y-2">
      <p className="text-sm text-muted-foreground">
        Gives this server and every worker a private address on an encrypted
        WireGuard network. This server must accept UDP{' '}
        <code>{mesh.listen_port}</code> from the internet. Once on, it stays on.
      </p>
      <Button
        size="sm"
        onClick={() => enable.mutate({ body: {} })}
        disabled={!mesh.can_enable}
      >
        <ShieldCheck className="mr-1 h-4 w-4" />
        Turn on private network
      </Button>
      {verificationDialog}
    </div>
  )
}

function JoinCommandStep({ url }: { url: string }) {
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()
  // The nodes present when the command was created; a new one is the worker.
  const knownNodes = useRef<Set<number> | null>(null)
  const mint = useMutation({
    ...mintEnrollmentTokenMutation(),
    onError: (error, variables) => {
      if (handleSensitiveActionError(error, () => mint.mutate(variables)))
        return
      toast.error('Could not create the join command', {
        description: problemDetail(error, 'Try again.'),
      })
    },
  })
  const waiting = !!mint.data
  const { data: nodeList } = useQuery({
    ...adminListNodesOptions(),
    refetchInterval: waiting ? WAITING_POLL_MS : false,
  })
  const joined = waiting
    ? (nodeList?.nodes ?? []).filter(
        (node) => knownNodes.current && !knownNodes.current.has(node.id)
      )
    : []
  const [created, setCreated] = useState(0)

  const create = () => {
    knownNodes.current = new Set((nodeList?.nodes ?? []).map((n) => n.id))
    setCreated((n) => n + 1)
    mint.mutate({ body: { max_uses: 1, ttl_secs: ENROLLMENT_TOKEN_TTL_SECS } })
  }

  if (!mint.data) {
    return (
      <div className="space-y-2">
        <p className="text-sm text-muted-foreground">
          Creates a command to run as root on the new machine. It installs
          Temps, joins this cluster and starts the agent.
        </p>
        <Button size="sm" onClick={create} disabled={mint.isPending}>
          {mint.isPending ? (
            <Loader2 className="mr-1 h-4 w-4 animate-spin" />
          ) : (
            <Key className="mr-1 h-4 w-4" />
          )}
          Create join command
        </Button>
        {verificationDialog}
      </div>
    )
  }

  const command = [
    'curl -fsSL https://temps.sh/install.sh | bash',
    outboundJoinCommand(url, mint.data.token),
    'temps agent service install',
  ].join('\n')

  return (
    <div className="space-y-2" key={created}>
      <p className="text-sm text-muted-foreground">
        Run this as root on the new machine:
      </p>
      <div className="flex items-start gap-2 rounded-md bg-muted px-3 py-2 font-mono text-xs">
        <pre className="min-w-0 flex-1 overflow-x-auto whitespace-pre">
          {command}
        </pre>
        <CopyButton minimal className="h-6 w-6 shrink-0" value={command} />
      </div>
      <p className="text-xs text-muted-foreground">
        Shown once. It adds one machine and expires{' '}
        <TimeAgo date={mint.data.expires_at} />.{' '}
        <button
          type="button"
          className="font-medium underline"
          onClick={create}
          disabled={mint.isPending}
        >
          Create another
        </button>
      </p>
      {joined.length > 0 ? (
        <p className="flex items-center gap-2 text-sm text-green-700 dark:text-green-400">
          <CheckCircle2 className="h-4 w-4" />
          {joined.map((node) => node.name).join(', ')} joined. It appears in the
          list below.
        </p>
      ) : (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          Waiting for the machine to connect…
        </p>
      )}
      {verificationDialog}
    </div>
  )
}
