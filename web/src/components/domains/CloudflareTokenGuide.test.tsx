// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import {
  CLOUDFLARE_API_TOKENS_URL,
  CloudflareTokenChecklist,
} from './CloudflareTokenGuide'

describe('CloudflareTokenChecklist', () => {
  test('lists every permission and the resources to limit it to', () => {
    const html = renderToStaticMarkup(<CloudflareTokenChecklist />)
    expect(html).toContain('Zone · Zone · Read')
    expect(html).toContain('Zone · DNS · Edit')
    expect(html).toContain('Account · Cloudflare Tunnel · Edit')
    expect(html).toContain('to create the tunnel and run it on your workers')
    expect(html).toContain('Account Resources')
    expect(html).toContain('Zone Resources')
    expect(html).toContain(CLOUDFLARE_API_TOKENS_URL)
  })

  test('marks the tunnel permission optional where a tunnel is optional', () => {
    const html = renderToStaticMarkup(
      <CloudflareTokenChecklist tunnel="optional" />
    )
    expect(html).toContain(
      'only if workers take traffic through a Cloudflare Tunnel'
    )
    expect(html).not.toContain('to create the tunnel')
  })
})
