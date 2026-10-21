// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { client } from '@/api/client/client.gen'
import type {
  AutofixCapabilityResponse,
  AutofixSettingsResponse,
} from '@/api/client/types.gen'
import { installBrowserWindow } from '@/test/dom'
import { AutofixSettingsCard } from './AutofixSettingsCard'

installBrowserWindow()

const originalConfig = client.getConfig()

afterEach(() => {
  cleanup()
  client.setConfig(originalConfig)
})

const PROJECT = 7

/**
 * A tiny autofix backend: the capability read returns whatever the last
 * write left behind, so a pause toggle or a save is followed by a refetch
 * that hands the card a fresh `settings` object, as the real server does.
 */
function serve() {
  let settings: AutofixSettingsResponse = {
    project_id: PROJECT,
    mode: 'off',
    min_confidence: 4,
    paused: false,
    protected_paths: [],
    test_command: null,
  }
  const writes: Array<{ path: string; body: unknown }> = []
  const capability = (): AutofixCapabilityResponse => ({
    configured: true,
    delivery: 'pull_request',
    source_kind: 'github',
    settings,
  })
  client.setConfig({
    baseUrl: 'https://console.example.test/api',
    fetch: Object.assign(
      async (input: RequestInfo | URL) => {
        const request = input instanceof Request ? input : new Request(input)
        const path = new URL(request.url).pathname
        if (path === `/api/projects/${PROJECT}/autofix`) {
          return Response.json(capability())
        }
        if (path === `/api/projects/${PROJECT}/autofix/pause`) {
          const body = (await request.json()) as { paused: boolean }
          writes.push({ path, body })
          settings = { ...settings, paused: body.paused }
          return Response.json(settings)
        }
        if (path === `/api/projects/${PROJECT}/autofix/settings`) {
          const body = (await request.json()) as {
            mode: string
            min_confidence: number
            protected_paths: string[]
            test_command: string
          }
          writes.push({ path, body })
          settings = { ...settings, ...body }
          return Response.json(settings)
        }
        return new Promise<Response>(() => {})
      },
      { preconnect: fetch.preconnect }
    ),
  })
  return writes
}

function renderCard() {
  const queries = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
  return render(
    <QueryClientProvider client={queries}>
      <MemoryRouter>
        <AutofixSettingsCard projectId={PROJECT} />
      </MemoryRouter>
    </QueryClientProvider>
  )
}

test('pausing autofix keeps the policy edits that are not saved yet', async () => {
  const writes = serve()
  renderCard()

  const command = (await screen.findByPlaceholderText(
    'npm test'
  )) as HTMLInputElement
  fireEvent.change(command, { target: { value: 'bun test' } })
  const paths = screen.getByRole('textbox', {
    name: 'Protected paths',
  }) as HTMLTextAreaElement
  fireEvent.change(paths, { target: { value: 'migrations/**' } })

  const toggle = screen.getByRole('switch', { name: 'Pause autofix' })
  fireEvent.click(toggle)

  // The pause round-trips and the capability is read again.
  await waitFor(() => expect(toggle.getAttribute('aria-checked')).toBe('true'))
  expect(writes.map((w) => w.path)).toEqual([
    `/api/projects/${PROJECT}/autofix/pause`,
  ])
  expect(command.value).toBe('bun test')
  expect(paths.value).toBe('migrations/**')
})

test('saving makes the saved values the form baseline', async () => {
  const writes = serve()
  renderCard()

  const command = (await screen.findByPlaceholderText(
    'npm test'
  )) as HTMLInputElement
  fireEvent.change(command, { target: { value: '  bun test  ' } })
  fireEvent.click(screen.getByRole('button', { name: 'Save' }))

  // The trimmed value the server stored replaces what was typed.
  await waitFor(() => expect(command.value).toBe('bun test'))
  expect(writes).toHaveLength(1)
  expect(writes[0]?.body).toMatchObject({ test_command: 'bun test' })

  // A later refetch (here, after pausing) leaves the saved value in place.
  const toggle = screen.getByRole('switch', { name: 'Pause autofix' })
  fireEvent.click(toggle)
  await waitFor(() => expect(toggle.getAttribute('aria-checked')).toBe('true'))
  expect(command.value).toBe('bun test')
})
