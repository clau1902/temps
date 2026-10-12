// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter, Route, Routes } from 'react-router'
import { cleanup, render } from '@testing-library/react'
import { BreadcrumbProvider } from '@/contexts/BreadcrumbContext'
import { shouldRetryQuery } from '@/lib/query-retry'
import { useBrowserWindow } from '@/test/dom'
import { MajorUpgradeDetail } from './MajorUpgradeDetail'

useBrowserWindow()

const originalFetch = globalThis.fetch
afterEach(() => {
  cleanup()
  globalThis.fetch = originalFetch
})

function respondWith(status: number, body: unknown) {
  const requested: string[] = []
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    requested.push(String(input))
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'Content-Type': 'application/json' },
    })
  }) as typeof fetch
  return requested
}

function renderAt(path: string) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: shouldRetryQuery } },
  })
  return render(
    <QueryClientProvider client={client}>
      <BreadcrumbProvider>
        <MemoryRouter initialEntries={[path]}>
          <Routes>
            <Route
              path="/storage/:id/upgrades/:upgradeId"
              element={<MajorUpgradeDetail />}
            />
          </Routes>
        </MemoryRouter>
      </BreadcrumbProvider>
    </QueryClientProvider>
  )
}

test('an unknown upgrade id shows a not-found state instead of loading forever', async () => {
  respondWith(404, { title: 'Not Found', detail: 'Upgrade 77 not found' })
  const view = renderAt('/storage/3/upgrades/77')
  expect(await view.findByText('Upgrade not found')).toBeTruthy()
  expect(
    view.getByText(/Upgrade #77 does not exist for service 3/)
  ).toBeTruthy()
})

test('a non-numeric id is reported as invalid without any request', async () => {
  const requested = respondWith(200, {})
  const view = renderAt('/storage/3/upgrades/abc')
  expect(await view.findByText('Invalid upgrade')).toBeTruthy()
  expect(requested).toHaveLength(0)
})
