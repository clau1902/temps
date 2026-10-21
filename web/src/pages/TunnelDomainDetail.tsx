// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// A domain a tunnel carries (ADR 058 D10), opened from the Domains list.
// `*.zone` is the tunnel itself: where it runs and what it publishes. An
// exact name is one hostname: whether it is live, why not, and its route.

import { useEffect } from 'react'
import { Link, useParams } from 'react-router'
import { useQuery } from '@tanstack/react-query'
import { ExternalLink } from 'lucide-react'
import {
  getIngressHostnameOptions,
  listIngressConnectorsOptions,
} from '@/api/client/@tanstack/react-query.gen'
import type { ConnectorView, HostnameView } from '@/api/client/types.gen'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import {
  HostnameChoice,
  StateBadge,
  TunnelHostnames,
} from '@/components/domains/TunnelHostnames'
import { TunnelLinks } from '@/components/nodes/TunnelLinks'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { problemDetail } from '@/lib/api-problem'

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid gap-1 sm:grid-cols-[180px_1fr]">
      <dt className="text-sm text-muted-foreground">{label}</dt>
      <dd className="min-w-0 break-words text-sm">{children}</dd>
    </div>
  )
}

const HTTPS_NOTE =
  'Cloudflare serves HTTPS and renews the certificate. Its free certificate ' +
  'covers the zone and one level below it.'

function ZoneDetail({ connector }: { connector: ConnectorView }) {
  return (
    <>
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Tunnel {connector.name}</CardTitle>
          <CardDescription>
            Apps under {connector.domain} reach visitors through this tunnel.
            Temps writes each app&apos;s DNS record; workers need no public
            address or open port.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <dl className="space-y-3">
            <Fact label="Status">
              {connector.status === 'error' ? (
                <span className="text-destructive">
                  {connector.last_error ?? 'Failed'}
                </span>
              ) : (
                connector.status
              )}
            </Fact>
            <Fact label="HTTPS">{HTTPS_NOTE}</Fact>
            {connector.dns_target && (
              <Fact label="DNS records point at">
                <code className="text-xs">{connector.dns_target}</code>
              </Fact>
            )}
            <Fact label="Runs on">
              {connector.nodes.length === 0 ? (
                <span className="text-muted-foreground">
                  No worker. Pick one from a worker&apos;s page.
                </span>
              ) : (
                <ul className="space-y-2">
                  {connector.nodes.map((node) => (
                    <li key={node.node_id} className="space-y-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <Link
                          to={`/settings/nodes/${node.node_id}`}
                          className="hover:underline"
                        >
                          {node.node_name}
                        </Link>
                        <Badge
                          variant={
                            node.state === 'ready'
                              ? 'default'
                              : node.state === 'error'
                                ? 'destructive'
                                : 'secondary'
                          }
                          className="text-xs"
                        >
                          {node.state === 'ready' ? 'connected' : node.state}
                        </Badge>
                      </div>
                      {node.last_error && node.state !== 'ready' && (
                        <p className="text-xs text-muted-foreground">
                          {node.last_error}
                        </p>
                      )}
                      {node.state === 'ready' && (
                        <TunnelLinks
                          connector={connector}
                          nodeId={node.node_id}
                          readyConnections={node.ready_connections}
                        />
                      )}
                    </li>
                  ))}
                </ul>
              )}
            </Fact>
          </dl>
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Hostnames</CardTitle>
          <CardDescription>
            What it publishes, and how each name reaches visitors.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <TunnelHostnames connector={connector} />
        </CardContent>
      </Card>
    </>
  )
}

function HostnameDetail({ view }: { view: HostnameView }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center gap-2 text-base">
          {view.state === 'published' ? (
            <a
              href={`https://${view.hostname}`}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 hover:underline"
            >
              {view.hostname} <ExternalLink className="h-3.5 w-3.5" />
            </a>
          ) : (
            view.hostname
          )}
          <StateBadge view={view} />
        </CardTitle>
        <CardDescription>{view.detail}</CardDescription>
      </CardHeader>
      <CardContent>
        <dl className="space-y-3">
          <Fact label="Reaches visitors">
            <HostnameChoice view={view} />
          </Fact>
          <Fact label="Tunnel">
            {view.connector_name ?? (
              <span className="text-muted-foreground">None</span>
            )}
          </Fact>
          <Fact label="Served by an app">
            {view.served_by_workers ? (
              'Yes, on a worker'
            ) : (
              <span className="text-muted-foreground">
                Not yet. Add it to an environment&apos;s domains and deploy
                on a worker.
              </span>
            )}
          </Fact>
          <Fact label="HTTPS">{HTTPS_NOTE}</Fact>
          {view.published_at && (
            <Fact label="Published">
              {new Date(view.published_at).toLocaleString()}
            </Fact>
          )}
        </dl>
      </CardContent>
    </Card>
  )
}

export function TunnelDomainDetail() {
  const { hostname = '' } = useParams<{ hostname: string }>()
  const zone = hostname.startsWith('*.') ? hostname.slice(2) : undefined
  const { setBreadcrumbs } = useBreadcrumbs()

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Domains', href: '/domains' },
      { label: hostname },
    ])
  }, [setBreadcrumbs, hostname])
  usePageTitle(`${hostname} - Domain Details`)

  const connectorsQuery = useQuery({
    ...listIngressConnectorsOptions(),
    enabled: !!zone,
    refetchInterval: 15_000,
    retry: false,
  })
  const hostnameQuery = useQuery({
    ...getIngressHostnameOptions({ path: { hostname } }),
    enabled: !zone && !!hostname,
    refetchInterval: 15_000,
    retry: false,
  })
  const query = zone ? connectorsQuery : hostnameQuery
  const connector = zone
    ? connectorsQuery.data?.find((c) => c.domain === zone)
    : undefined

  return (
    <div className="space-y-6">
      <div>
        <h1 className="break-all text-2xl font-semibold">{hostname}</h1>
        <p className="text-sm text-muted-foreground">
          {zone
            ? 'Every name under this zone, through a tunnel'
            : 'Through a tunnel'}
        </p>
      </div>
      {query.isLoading ? (
        <Skeleton className="h-48 w-full" />
      ) : query.isError ? (
        <Alert variant="destructive">
          <AlertTitle>Could not load {hostname}</AlertTitle>
          <AlertDescription>
            {problemDetail(query.error, 'Try again in a moment.')}
          </AlertDescription>
        </Alert>
      ) : zone ? (
        connector ? (
          <ZoneDetail connector={connector} />
        ) : (
          <Alert>
            <AlertTitle>No tunnel covers {zone}</AlertTitle>
            <AlertDescription>
              It may have been deleted. <Link to="/domains">Back to Domains</Link>
            </AlertDescription>
          </Alert>
        )
      ) : hostnameQuery.data ? (
        <HostnameDetail view={hostnameQuery.data} />
      ) : null}
    </div>
  )
}
