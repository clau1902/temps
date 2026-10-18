// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { ModelVendorTabs } from './AssistantModelPicker'
import { ModelVendorLogo } from './ModelVendorLogo'

const tabs = [
  { key: 'google', name: 'Google', logo: 'google.svg', count: 44 },
  { key: 'qwen', name: 'Qwen', logo: null, count: 43 },
]

test('the tabs show the vendors with their marks and counts, All selected', () => {
  const markup = renderToStaticMarkup(
    <ModelVendorTabs
      tabs={tabs}
      total={87}
      selected={null}
      searching={false}
      onSelect={() => {}}
    />
  )
  expect(markup).toContain('role="tablist"')
  expect(markup).toContain('All')
  expect(markup).toContain('87')
  expect(markup).toContain('/ai-agents/google.svg')
  expect(markup).toContain('Qwen')
  expect(markup.match(/aria-selected="true"/g)).toHaveLength(1)
})

test('a single vendor needs no tabs', () => {
  const markup = renderToStaticMarkup(
    <ModelVendorTabs
      tabs={tabs.slice(0, 1)}
      total={44}
      selected={null}
      searching={false}
      onSelect={() => {}}
    />
  )
  expect(markup).toBe('')
})

test('a vendor without a mark gets its initial, an unnamed one a generic icon', () => {
  const lettered = renderToStaticMarkup(
    <ModelVendorLogo vendor={{ name: 'Qwen', logo: null }} />
  )
  expect(lettered).toContain('>Q<')
  expect(lettered).not.toContain('<img')
  const generic = renderToStaticMarkup(
    <ModelVendorLogo vendor={{ name: 'Other', logo: null }} />
  )
  expect(generic).toContain('<svg')
  const marked = renderToStaticMarkup(
    <ModelVendorLogo vendor={{ name: 'Google', logo: 'google.svg' }} />
  )
  expect(marked).toContain('<img')
})
