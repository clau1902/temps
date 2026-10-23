// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// The least-privilege Cloudflare API token Temps needs, as the operator
// creates it in the Cloudflare dashboard. Temps only lists zones, edits DNS
// records and, for tunnels, manages the account's Cloudflare Tunnels.

import { ExternalLink } from 'lucide-react'

export const CLOUDFLARE_API_TOKENS_URL =
  'https://dash.cloudflare.com/profile/api-tokens'

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
