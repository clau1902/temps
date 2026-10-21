// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// Tunnels on a worker (ADR 058 D8). Everything provider-specific comes from
// the provider's descriptor, so a new provider needs no code here.

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { CheckCircle2, Loader2, RefreshCw, Trash2, XCircle } from 'lucide-react'
import { useState } from 'react'
import { Link } from 'react-router'
import { toast } from 'sonner'
import {
  createIngressConnectorMutation,
  deleteIngressConnectorMutation,
  listDnsProvidersOptions,
  listIngressConnectorsOptions,
  listIngressProvidersOptions,
  listManagedDomainsOptions,
  setIngressConnectorNodesMutation,
  syncIngressConnectorMutation,
} from '@/api/client/@tanstack/react-query.gen'
import type {
  ConnectorNodeView,
  ConnectorView,
  NodeInfoResponse,
  ProviderInfo,
  SetupField,
} from '@/api/client/types.gen'
import { TunnelHostnames } from '@/components/domains/TunnelHostnames'
import { TunnelLinks } from '@/components/nodes/TunnelLinks'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { ConfirmDeleteDialog } from '@/components/ui/confirm-delete-dialog'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { useSensitiveActionVerification } from '@/hooks/useSensitiveActionVerification'
import { problemDetail } from '@/lib/api-problem'

/** What a replica's reported state means, for the operator. */
export function replicaState(replica: ConnectorNodeView): {
  label: string
  ok: boolean
} {
  switch (replica.state) {
    case 'ready':
      return { label: 'Connected', ok: true }
    case 'starting':
      return { label: 'Starting', ok: false }
    case 'pending':
      return { label: 'Waiting for the worker to pick it up', ok: false }
    case 'refused':
      return { label: 'Refused by the worker', ok: false }
    case 'degraded':
      return { label: 'Degraded', ok: false }
    default:
      return { label: 'Not running', ok: false }
  }
}

export function WorkerTunnels({
  node,
  allowed,
  recommended,
}: {
  node: NodeInfoResponse
  /** Whether the worker delegated ingress; tunnels need that too. */
  allowed: boolean
  /** Lead with tunnels: the worker has no address the internet can reach. */
  recommended: boolean
}) {
  const queryClient = useQueryClient()
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()
  const providersQuery = useQuery(listIngressProvidersOptions())
  const connectorsQuery = useQuery({
    ...listIngressConnectorsOptions(),
    // Replicas report every few seconds.
    refetchInterval: 10_000,
  })
  const [setupFor, setSetupFor] = useState<ProviderInfo | null>(null)
  const [deleting, setDeleting] = useState<ConnectorView | null>(null)

  const refresh = () =>
    queryClient.invalidateQueries({
      queryKey: listIngressConnectorsOptions().queryKey,
    })
  const failed = (title: string) => (error: Error) =>
    toast.error(title, {
      description: problemDetail(error, 'Try again in a moment.'),
    })

  const setNodes = useMutation({
    ...setIngressConnectorNodesMutation(),
    onSuccess: refresh,
    onError: (error, variables) => {
      if (handleSensitiveActionError(error, () => setNodes.mutate(variables)))
        return
      failed('Could not change where the tunnel runs')(error)
    },
  })
  const sync = useMutation({
    ...syncIngressConnectorMutation(),
    onSuccess: async () => {
      toast.success('Tunnel checked and republished')
      await refresh()
    },
    onError: failed('Could not sync the tunnel'),
  })
  const remove = useMutation({
    ...deleteIngressConnectorMutation(),
    onSuccess: async () => {
      toast.success('Tunnel deleted')
      setDeleting(null)
      await refresh()
    },
    onError: (error, variables) => {
      if (handleSensitiveActionError(error, () => remove.mutate(variables)))
        return
      failed('Could not delete the tunnel')(error)
    },
  })

  const providers = providersQuery.data ?? []
  const connectors = connectorsQuery.data ?? []
  const here = connectors.filter((c) =>
    c.nodes.some((n) => n.node_id === node.id)
  )
  const elsewhere = connectors.filter(
    (c) => !c.nodes.some((n) => n.node_id === node.id)
  )
  const providerName = (kind: string) =>
    providers.find((p) => p.kind === kind)?.name ?? kind

  if (providersQuery.isLoading || connectorsQuery.isLoading) {
    return (
      <p className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" /> Loading tunnels…
      </p>
    )
  }

  return (
    <div className="space-y-3">
      {here.map((connector) => {
        const replica = connector.nodes.find((n) => n.node_id === node.id)!
        const state = replicaState(replica)
        return (
          <div key={connector.id} className="space-y-2 rounded-md border p-3">
            <div className="flex flex-wrap items-start justify-between gap-2">
              <div className="min-w-0 space-y-0.5">
                <div className="flex items-center gap-2 text-sm font-medium">
                  {connector.name}
                  <Badge variant="outline">
                    {providerName(connector.provider)}
                  </Badge>
                </div>
                <p className="text-xs text-muted-foreground">
                  Apps under {connector.domain}
                  {connector.nodes.length > 1 &&
                    ` · also runs on ${connector.nodes.length - 1} other worker${connector.nodes.length > 2 ? 's' : ''}`}
                </p>
              </div>
              <div className="flex items-center gap-1">
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={sync.isPending}
                  onClick={() => sync.mutate({ path: { id: connector.id } })}
                >
                  <RefreshCw className="mr-1 h-4 w-4" /> Sync
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={setNodes.isPending}
                  onClick={() =>
                    setNodes.mutate({
                      path: { id: connector.id },
                      body: {
                        node_ids: connector.nodes
                          .map((n) => n.node_id)
                          .filter((id) => id !== node.id),
                      },
                    })
                  }
                >
                  Stop here
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  aria-label={`Delete tunnel ${connector.name}`}
                  onClick={() => setDeleting(connector)}
                >
                  <Trash2 className="h-4 w-4" />
                </Button>
              </div>
            </div>
            {connector.status === 'error' ? (
              <StatusLine ok={false}>
                {connector.last_error ?? 'The provider refused the tunnel.'}
              </StatusLine>
            ) : connector.status === 'pending' ? (
              <StatusLine ok={false}>Creating the tunnel…</StatusLine>
            ) : (
              <>
                <StatusLine ok={state.ok}>
                  {state.label}
                  {replica.last_error ? `: ${replica.last_error}` : ''}
                </StatusLine>
                {replica.state === 'ready' && (
                  <TunnelLinks
                    connector={connector}
                    nodeId={node.id}
                    readyConnections={replica.ready_connections}
                  />
                )}
              </>
            )}
            <TunnelHostnames connector={connector} />
          </div>
        )
      })}

      {allowed && here.length === 0 && elsewhere.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <span className="text-muted-foreground">
            Run an existing tunnel here:
          </span>
          {elsewhere.map((connector) => (
            <Button
              key={connector.id}
              size="sm"
              variant="outline"
              disabled={setNodes.isPending}
              onClick={() =>
                setNodes.mutate({
                  path: { id: connector.id },
                  body: {
                    node_ids: [
                      ...connector.nodes.map((n) => n.node_id),
                      node.id,
                    ],
                  },
                })
              }
            >
              {connector.name} ({connector.domain})
            </Button>
          ))}
        </div>
      )}

      {allowed && here.length === 0 && (
        <div className="grid gap-3 md:grid-cols-2">
          {providers.map((provider) => (
            <div
              key={provider.kind}
              className="flex flex-col gap-2 rounded-md border p-3"
            >
              <div className="flex flex-wrap items-center gap-2 text-sm font-medium">
                {provider.name}
                {recommended && !provider.needs_public_ip && (
                  <Badge>Recommended</Badge>
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                {provider.summary}
              </p>
              <ul className="list-disc space-y-0.5 pl-4 text-xs text-muted-foreground">
                {provider.requirements.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
              <div className="mt-auto flex items-center gap-2 pt-1">
                <Button size="sm" onClick={() => setSetupFor(provider)}>
                  Set up
                </Button>
                {provider.docs_url && (
                  <Button size="sm" variant="link" asChild className="px-0">
                    <a
                      href={provider.docs_url}
                      target="_blank"
                      rel="noreferrer"
                    >
                      How it works
                    </a>
                  </Button>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      {setupFor && (
        <ProviderSetupDialog
          provider={setupFor}
          node={node}
          onClose={() => setSetupFor(null)}
          onCreated={refresh}
        />
      )}
      <ConfirmDeleteDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        resourceType="tunnel"
        resourceName={deleting?.name ?? ''}
        consequences={
          deleting &&
          `Apps under ${deleting.domain} stop being reachable through it, on every worker that runs it. Temps removes the tunnel and its DNS records at ${providerName(deleting.provider)}.`
        }
        onConfirm={() => deleting && remove.mutate({ path: { id: deleting.id } })}
      />
      {verificationDialog}
    </div>
  )
}

function StatusLine({
  ok,
  children,
}: {
  ok: boolean
  children: React.ReactNode
}) {
  return (
    <p className="flex items-start gap-2 text-sm">
      {ok ? (
        <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-green-600" />
      ) : (
        <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
      )}
      <span>{children}</span>
    </p>
  )
}

function defaultName(node: NodeInfoResponse) {
  const name = node.name
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, '-')
    .replace(/^-+|-+$/g, '')
  return (name || `worker-${node.id}`).slice(0, 50)
}

/** The setup form, rendered from the provider's fields. */
function ProviderSetupDialog({
  provider,
  node,
  onClose,
  onCreated,
}: {
  provider: ProviderInfo
  node: NodeInfoResponse
  onClose: () => void
  onCreated: () => Promise<unknown>
}) {
  const { handleSensitiveActionError, verificationDialog } =
    useSensitiveActionVerification()
  const [name, setName] = useState(defaultName(node))
  const [values, setValues] = useState<Record<string, string>>({})
  const set = (key: string, value: string) =>
    setValues((current) => {
      const next = { ...current, [key]: value }
      // A domain belongs to the DNS provider it was picked under.
      for (const field of provider.fields) {
        if (
          field.kind.type === 'managed_domain' &&
          field.kind.provider_field === key
        ) {
          delete next[field.key]
        }
      }
      return next
    })

  const create = useMutation({
    ...createIngressConnectorMutation(),
    onSuccess: async (connector) => {
      toast.success(
        connector.status === 'ready'
          ? `${provider.name} created. The worker connects within a few seconds.`
          : `${provider.name} saved`
      )
      await onCreated()
      onClose()
    },
    onError: (error, variables) => {
      if (handleSensitiveActionError(error, () => create.mutate(variables)))
        return
      toast.error(`Could not set up ${provider.name}`, {
        description: problemDetail(error, 'Check the answers and try again.'),
      })
    },
  })

  const missing = provider.fields.some(
    (f) => f.required && !values[f.key]?.trim()
  )

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Set up {provider.name}</DialogTitle>
          <DialogDescription>{provider.summary}</DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-1.5">
            <Label htmlFor="tunnel-name">Name</Label>
            <Input
              id="tunnel-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">
              Lowercase letters, digits and hyphens. Other workers can run the
              same tunnel later for failover.
            </p>
          </div>
          {provider.fields.map((field) => (
            <SetupFieldInput
              key={field.key}
              field={field}
              values={values}
              onChange={(value) => set(field.key, value)}
            />
          ))}
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            disabled={missing || !name.trim() || create.isPending}
            onClick={() =>
              create.mutate({
                body: {
                  name: name.trim(),
                  provider: provider.kind,
                  values,
                  node_ids: [node.id],
                },
              })
            }
          >
            {create.isPending && (
              <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            )}
            Create and run on {node.name}
          </Button>
        </DialogFooter>
        {verificationDialog}
      </DialogContent>
    </Dialog>
  )
}

function SetupFieldInput({
  field,
  values,
  onChange,
}: {
  field: SetupField
  values: Record<string, string>
  onChange: (value: string) => void
}) {
  const id = `setup-${field.key}`
  const value = values[field.key] ?? ''
  let control: React.ReactNode
  switch (field.kind.type) {
    case 'dns_provider':
      control = (
        <DnsProviderSelect
          id={id}
          providerType={field.kind.provider_type ?? null}
          value={value}
          onChange={onChange}
        />
      )
      break
    case 'managed_domain':
      control = (
        <ManagedDomainSelect
          key={values[field.kind.provider_field] ?? ''}
          id={id}
          providerId={Number(values[field.kind.provider_field]) || null}
          allowSubdomain={field.kind.allow_subdomain ?? false}
          onChange={onChange}
        />
      )
      break
    default:
      control = (
        <Input
          id={id}
          type={field.kind.type === 'secret' ? 'password' : 'text'}
          autoComplete="off"
          placeholder={field.placeholder ?? undefined}
          value={value}
          onChange={(e) => onChange(e.target.value)}
        />
      )
  }
  return (
    <div className="space-y-1.5">
      <Label htmlFor={id}>{field.label}</Label>
      {control}
      {field.help && (
        <p className="text-xs text-muted-foreground">{field.help}</p>
      )}
    </div>
  )
}

function DnsProviderSelect({
  id,
  providerType,
  value,
  onChange,
}: {
  id: string
  providerType: string | null
  value: string
  onChange: (value: string) => void
}) {
  const query = useQuery(listDnsProvidersOptions())
  const providers = (query.data ?? []).filter(
    (p) =>
      p.is_active &&
      (!providerType || p.provider_type.toLowerCase() === providerType)
  )
  if (query.isLoading) {
    return <p className="text-sm text-muted-foreground">Loading…</p>
  }
  if (providers.length === 0) {
    return (
      <Alert>
        <AlertDescription>
          No {providerType ? `${providerType} ` : ''}DNS provider yet.{' '}
          <Link className="underline" to="/dns-providers/add">
            Add one
          </Link>
          , then come back.
        </AlertDescription>
      </Alert>
    )
  }
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger id={id}>
        <SelectValue placeholder="Choose…" />
      </SelectTrigger>
      <SelectContent>
        {providers.map((p) => (
          <SelectItem key={p.id} value={String(p.id)}>
            {p.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

function ManagedDomainSelect({
  id,
  providerId,
  allowSubdomain,
  onChange,
}: {
  id: string
  providerId: number | null
  allowSubdomain: boolean
  onChange: (value: string) => void
}) {
  const query = useQuery({
    ...listManagedDomainsOptions({ path: { id: providerId ?? 0 } }),
    enabled: providerId != null,
  })
  const [zone, setZone] = useState('')
  const [subdomain, setSubdomain] = useState('')
  const pick = (nextZone: string, nextSubdomain: string) => {
    setZone(nextZone)
    setSubdomain(nextSubdomain)
    const label = nextSubdomain.trim().toLowerCase().replace(/\.+$/, '')
    onChange(nextZone && label ? `${label}.${nextZone}` : nextZone)
  }
  if (providerId == null) {
    return (
      <Select disabled>
        <SelectTrigger id={id}>
          <SelectValue placeholder="Choose the account first" />
        </SelectTrigger>
      </Select>
    )
  }
  if (query.isLoading) {
    return <p className="text-sm text-muted-foreground">Loading…</p>
  }
  const domains = query.data ?? []
  if (domains.length === 0) {
    return (
      <Alert>
        <AlertDescription>
          This DNS provider manages no domain yet.{' '}
          <Link className="underline" to={`/dns-providers/${providerId}`}>
            Add one
          </Link>
          , then come back.
        </AlertDescription>
      </Alert>
    )
  }
  const label = subdomain.trim().toLowerCase().replace(/\.+$/, '')
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        {allowSubdomain && (
          <>
            <Input
              aria-label="Subdomain (optional)"
              className="w-40"
              placeholder="subdomain"
              autoComplete="off"
              value={subdomain}
              onChange={(e) => pick(zone, e.target.value)}
            />
            <span className="text-muted-foreground">.</span>
          </>
        )}
        <Select value={zone} onValueChange={(next) => pick(next, subdomain)}>
          <SelectTrigger id={id} className="flex-1">
            <SelectValue placeholder="Choose…" />
          </SelectTrigger>
          <SelectContent>
            {domains.map((d) => (
              <SelectItem key={d.id} value={d.domain}>
                {d.domain}
                {!d.verified && ' (not verified)'}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      {allowSubdomain && !label && (
        <p className="text-xs text-muted-foreground">
          Leave the subdomain empty to use the whole domain, or enter one (e.g.{' '}
          <code>lab</code>) to keep this tunnel to it.
        </p>
      )}
      {label && zone && (
        <Alert>
          <AlertDescription className="space-y-1 text-xs">
            <p>
              The tunnel serves{' '}
              <code>
                {label}.{zone}
              </code>{' '}
              and the hostnames under it, such as{' '}
              <code>
                app.{label}.{zone}
              </code>
              .
            </p>
            <p>
              Cloudflare's free certificate covers only one level below {zone}:{' '}
              <code>
                {label}.{zone}
              </code>{' '}
              works out of the box, but{' '}
              <code>
                app.{label}.{zone}
              </code>{' '}
              needs Cloudflare's Advanced Certificate Manager (or Total TLS) for{' '}
              <code>
                *.{label}.{zone}
              </code>
              , or visitors get a certificate error.
            </p>
          </AlertDescription>
        </Alert>
      )}
    </div>
  )
}
