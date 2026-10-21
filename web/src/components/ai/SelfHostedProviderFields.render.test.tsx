// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useState } from 'react'
import { MemoryRouter } from 'react-router'
import { client } from '@/api/client/client.gen'
import { installBrowserWindow } from '@/test/dom'
import {
  SelfHostedProviderFields,
  type SelfHostedDiscovery,
} from './SelfHostedProviderFields'
import { DEFAULT_SELF_HOSTED_CAPABILITIES } from './self-hosted-provider'

installBrowserWindow()

const originalConfig = client.getConfig()

afterEach(() => {
  cleanup()
  client.setConfig(originalConfig)
})

/** Answer model discovery with `answer`, recording each request body. */
function serveDiscovery(answer: () => Response) {
  const requests: unknown[] = []
  client.setConfig({
    baseUrl: 'https://console.example.test/api',
    fetch: Object.assign(
      async (input: RequestInfo | URL) => {
        const request = input instanceof Request ? input : null
        const url = request ? request.url : String(input)
        if (new URL(url).pathname === '/api/ai/providers/discover') {
          requests.push(request ? await request.json() : null)
          return answer()
        }
        return new Promise<Response>(() => {})
      },
      { preconnect: fetch.preconnect }
    ),
  })
  return requests
}

function Form({ baseUrl }: { baseUrl: string }) {
  const [discovery, setDiscovery] = useState<SelfHostedDiscovery | null>(null)
  const [defaultModel, setDefaultModel] = useState('')
  return (
    <SelfHostedProviderFields
      open
      baseUrl={baseUrl}
      apiKey=""
      trustPrivateAddress={false}
      onTrustPrivateAddressChange={() => {}}
      canTrustPrivateAddress
      discovery={discovery}
      onDiscovered={setDiscovery}
      defaultModel={defaultModel}
      onDefaultModelChange={setDefaultModel}
      capabilities={DEFAULT_SELF_HOSTED_CAPABILITIES}
      onCapabilitiesChange={() => {}}
    />
  )
}

function renderForm(baseUrl: string) {
  const queries = new QueryClient({
    defaultOptions: { mutations: { retry: false } },
  })
  return render(
    <QueryClientProvider client={queries}>
      <MemoryRouter>
        <Form baseUrl={baseUrl} />
      </MemoryRouter>
    </QueryClientProvider>
  )
}

test('a complete base URL loads the models without a click', async () => {
  const requests = serveDiscovery(() =>
    Response.json({
      models: [
        { id: 'model-a', name: 'Model A Flash' },
        { id: 'model-b', name: null },
      ],
    })
  )
  renderForm('https://llm.example.test/v1')

  await screen.findByText(/Connected: 2 models found/, undefined, {
    timeout: 3000,
  })
  expect(requests).toHaveLength(1)
  expect(requests[0]).toMatchObject({
    base_url: 'https://llm.example.test/v1',
  })
  // The first model becomes the default, shown by the name the server gave.
  expect(screen.getByRole('combobox').textContent).toContain('Model A Flash')
})

test('a server that cannot be listed shows why, once', async () => {
  const requests = serveDiscovery(() =>
    Response.json(
      {
        title: 'Discovery Failed',
        status: 502,
        detail: 'The server at llm.example.test refused the connection.',
      },
      { status: 502 }
    )
  )
  renderForm('https://llm.example.test/v1')

  const alert = await screen.findByRole('alert', undefined, { timeout: 3000 })
  expect(alert.textContent).toContain('refused the connection')
  expect(screen.getByRole('button', { name: /Reload models/ })).toBeTruthy()
  // The failure is shown, not retried in a loop.
  await new Promise((resolve) => setTimeout(resolve, 900))
  expect(requests).toHaveLength(1)
})

test('a half-typed URL sends no request', async () => {
  const requests = serveDiscovery(() => Response.json({ models: [] }))
  renderForm('https://')

  await new Promise((resolve) => setTimeout(resolve, 900))
  expect(requests).toHaveLength(0)
  expect(screen.getByText('Enter the base URL to load its models')).toBeTruthy()
})
