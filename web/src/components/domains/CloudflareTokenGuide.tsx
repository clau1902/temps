// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// The least-privilege Cloudflare API token Temps needs, as the operator
// creates it in the Cloudflare dashboard. Temps only lists zones, edits DNS
// records and, for tunnels, manages the account's Cloudflare Tunnels.

import { ExternalLink } from 'lucide-react'
import { cn } from '@/lib/utils'

export const CLOUDFLARE_API_TOKENS_URL =
  'https://dash.cloudflare.com/profile/api-tokens'

/** The token's permissions, in Cloudflare's own words, and what each is for. */
export const CLOUDFLARE_TOKEN_PERMISSIONS = [
  { permission: 'Zone · Zone · Read', purpose: 'find your domains' },
  { permission: 'Zone · DNS · Edit', purpose: 'create and remove DNS records' },
  {
    permission: 'Account · Cloudflare Tunnel · Edit',
    purpose: 'create the tunnel and run it on your workers',
    tunnelOnly: true,
  },
] as const

/**
 * What the token must have, at a glance: one line per permission and
 * resource. `CloudflareTokenGuide` walks through creating it.
 */
export function CloudflareTokenChecklist({
  tunnel = 'required',
  className,
}: {
  /** Whether the token must also manage Cloudflare Tunnels. */
  tunnel?: 'required' | 'optional'
  className?: string
}) {
  return (
    <div
      className={cn(
        'space-y-2 rounded-md border bg-muted/30 p-3 text-xs',
        className
      )}
    >
      <p className="text-sm font-medium">The Cloudflare API token needs</p>
      <div>
        <p className="font-medium text-muted-foreground">Permissions</p>
        <ul className="mt-1 space-y-1">
          {CLOUDFLARE_TOKEN_PERMISSIONS.map((entry) => (
            <li key={entry.permission} className="flex flex-wrap gap-x-2">
              <code className="rounded bg-muted px-1">{entry.permission}</code>
              <span className="text-muted-foreground">
                {'tunnelOnly' in entry && tunnel === 'optional'
                  ? 'only if workers take traffic through a Cloudflare Tunnel'
                  : `to ${entry.purpose}`}
              </span>
            </li>
          ))}
        </ul>
      </div>
      <div>
        <p className="font-medium text-muted-foreground">Limited to</p>
        <ul className="mt-1 list-disc space-y-0.5 pl-4 text-muted-foreground">
          <li>
            <strong className="text-foreground">Account Resources</strong>: the
            one account that holds your domain
          </li>
          <li>
            <strong className="text-foreground">Zone Resources</strong>: only
            the domains Temps manages, not &ldquo;All zones&rdquo;
          </li>
        </ul>
      </div>
      <a
        className="inline-flex items-center gap-0.5 text-foreground underline"
        href={CLOUDFLARE_API_TOKENS_URL}
        target="_blank"
        rel="noreferrer"
      >
        Create it in Cloudflare
        <ExternalLink className="h-3 w-3" />
      </a>
    </div>
  )
}

export function CloudflareTokenGuide({
  tunnel = 'optional',
}: {
  /** Whether the token must also manage Cloudflare Tunnels. */
  tunnel?: 'required' | 'optional'
}) {
  return (
    <div className="space-y-2 rounded-md border p-3 text-sm">
      <p className="font-medium">Create a token with only what Temps needs</p>
      <ol className="list-decimal space-y-1.5 pl-5 text-muted-foreground">
        <li>
          Open{' '}
          <a
            className="inline-flex items-center gap-0.5 text-foreground underline"
            href={CLOUDFLARE_API_TOKENS_URL}
            target="_blank"
            rel="noreferrer"
          >
            API Tokens
            <ExternalLink className="h-3 w-3" />
          </a>{' '}
          in Cloudflare, then <strong>Create Token</strong> →{' '}
          <strong>Create Custom Token</strong>. Don&apos;t use the Global API
          Key: it can do anything on your account.
        </li>
        <li>
          Permissions:
          <ul className="mt-1 list-disc space-y-0.5 pl-5">
            <li>
              <code>Zone · Zone · Read</code>, to find your domains
            </li>
            <li>
              <code>Zone · DNS · Edit</code>, to create and remove DNS records
            </li>
            <li>
              <code>Account · Cloudflare Tunnel · Edit</code>
              {tunnel === 'required'
                ? ', to create the tunnel and run it on your workers'
                : ', only if workers will take traffic through a Cloudflare Tunnel'}
            </li>
          </ul>
        </li>
        <li>
          <strong>Account Resources</strong>: Include → the one account that
          holds your domain.
        </li>
        <li>
          <strong>Zone Resources</strong>: Include → Specific zone → each domain
          Temps manages. Not &ldquo;All zones&rdquo;.
        </li>
        <li>
          Leave the expiry empty, or note it: when the token expires, Temps can
          no longer update DNS records
          {tunnel === 'required' ? ' or tunnels' : ''}.
        </li>
      </ol>
      <p className="text-xs text-muted-foreground">
        An existing token can be edited in Cloudflare to add a permission or a
        zone; it keeps its value, so Temps needs no change.
      </p>
    </div>
  )
}
