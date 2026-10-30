// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, beforeEach, expect, test } from 'bun:test'
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react'
import { useState } from 'react'
import { MemoryRouter } from 'react-router'
import { installBrowserWindow } from '@/test/dom'
import { useScrollToLinkedSection } from './useOpenSectionFromHash'

installBrowserWindow()
afterEach(cleanup)

let scrolledTo: string[] = []
const originalScroll = Element.prototype.scrollIntoView
beforeEach(() => {
  scrolledTo = []
  Element.prototype.scrollIntoView = function (this: Element) {
    scrolledTo.push(this.id)
  }
})
afterEach(() => {
  Element.prototype.scrollIntoView = originalScroll
})

/** A settings page whose card renders after its group, like a slow query. */
function Page() {
  useScrollToLinkedSection()
  const [loaded, setLoaded] = useState(false)
  return (
    <section id="settings-section-build">
      <button type="button" onClick={() => setLoaded(true)}>
        Load
      </button>
      {loaded && <div id="setting-docker-compose">Docker Compose</div>}
    </section>
  )
}

function renderAt(url: string) {
  const view = render(
    <MemoryRouter initialEntries={[url]}>
      <Page />
    </MemoryRouter>
  )
  return { showSetting: () => fireEvent.click(view.getByText('Load')) }
}

test('lands on the group first, then on the setting once it renders', async () => {
  const { showSetting } = renderAt(
    '/settings/delivery?section=build#setting-docker-compose'
  )
  await waitFor(() => expect(scrolledTo).toEqual(['settings-section-build']))
  showSetting()
  await waitFor(() =>
    expect(scrolledTo).toEqual([
      'settings-section-build',
      'setting-docker-compose',
    ])
  )
})

test('stays on the group when this service does not have the setting', async () => {
  const { showSetting } = renderAt(
    '/settings/delivery?section=build#setting-compose-services'
  )
  await waitFor(() => expect(scrolledTo).toEqual(['settings-section-build']))
  showSetting()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(scrolledTo).toEqual(['settings-section-build'])
})
