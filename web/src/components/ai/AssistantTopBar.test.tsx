// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { MemoryRouter } from 'react-router'

import { AssistantTopBar, CONSOLE_HOME_PATH } from './AssistantTopBar'

describe('AssistantTopBar', () => {
  const markup = renderToStaticMarkup(
    <MemoryRouter initialEntries={['/chat']}>
      <AssistantTopBar />
    </MemoryRouter>
  )

  test('offers a way back to the console, since the sidebar is not shown', () => {
    expect(markup).toContain(`href="${CONSOLE_HOME_PATH}"`)
    expect(markup).toContain('Back to console')
  })

  test('is the only chrome: no console navigation or page actions', () => {
    expect(markup).not.toContain('<nav')
    expect(markup).not.toContain('Sidebar')
  })
})
