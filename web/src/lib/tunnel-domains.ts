// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// Which tunnels can carry a domain (ADR 058 D10). A tunnel publishes names
// under its own domain; the wizard and the domain picker use this to offer
// "through the tunnel" instead of a certificate.

import type { ConnectorView } from '@/api/client/types.gen'

/** `*.Example.com.` → `example.com`; `App.example.com` → `app.example.com`. */
export function baseHost(host: string): string {
  return host
    .trim()
    .toLowerCase()
    .replace(/\.$/, '')
    .replace(/^\*\./, '')
}

/**
 * Tunnels whose domain covers `host` (an exact name or `*.domain`), the most
 * specific first and the oldest on a tie: the order the control plane picks
 * them in.
 */
export function tunnelsCovering(
  host: string,
  connectors: ConnectorView[]
): ConnectorView[] {
  const base = baseHost(host)
  if (!base) return []
  return connectors
    .filter((c) => {
      const domain = baseHost(c.domain)
      return base === domain || base.endsWith(`.${domain}`)
    })
    .sort((a, b) => b.domain.length - a.domain.length || a.id - b.id)
}

/**
 * How many labels the names under `host` sit below the tunnel's domain. The
 * tunnel's domain is usually the zone, and Cloudflare's free certificate only
 * covers one level below it.
 */
export function levelsBelow(host: string, connector: ConnectorView): number {
  const base = baseHost(host)
  const domain = baseHost(connector.domain)
  const own = base === domain ? 0 : base.slice(0, -domain.length - 1).split('.').length
  return host.trim().startsWith('*.') ? own + 1 : own
}

export const TUNNEL_PREFIX = 'tunnel:'

export function tunnelChoice(connectorId: number): string {
  return `${TUNNEL_PREFIX}${connectorId}`
}

export function chosenTunnel(value: string | undefined): number | null {
  if (!value?.startsWith(TUNNEL_PREFIX)) return null
  const id = Number(value.slice(TUNNEL_PREFIX.length))
  return Number.isInteger(id) ? id : null
}
