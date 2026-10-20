// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { WorkTabsOnboarding } from './WorkTabsOnboarding'

test('before a chat exists the work panel says what each tab will hold', () => {
  const markup = renderToStaticMarkup(<WorkTabsOnboarding />)
  expect(markup).toContain('aria-label="Working tabs"')
  for (const tab of ['Preview', 'Files', 'Changes', 'Terminal']) {
    expect(markup).toContain(tab)
  }
  expect(markup).toContain('barber shop')
})
