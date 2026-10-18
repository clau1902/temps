// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, it } from 'bun:test'
import type { AssistantModelOption } from './assistant-models'
import {
  filterByVendorTab,
  MAX_VENDOR_TABS,
  modelVendor,
  OTHER_VENDORS_KEY,
  vendorTabs,
} from './model-vendors'

const option = (id: string): AssistantModelOption => ({
  id,
  name: id,
  details: {},
})

describe('modelVendor', () => {
  it('reads the vendor from the id prefix and gives known ones their mark', () => {
    expect(modelVendor('google/gemini-3.5-flash')).toEqual({
      key: 'google',
      name: 'Google',
      logo: 'google.svg',
    })
    expect(modelVendor('mistralai/mistral-large').name).toBe('Mistral')
  })

  it('treats a router alias as its vendor and merges vendors that share a name', () => {
    expect(modelVendor('~openai/gpt-latest').key).toBe('openai')
    expect(modelVendor('meta/llama-x').key).toBe(
      modelVendor('meta-llama/llama-y').key
    )
  })

  it('names an unknown vendor from its slug and gives it no mark', () => {
    expect(modelVendor('sao10k/some-model')).toEqual({
      key: 'sao10k',
      name: 'Sao10k',
      logo: null,
    })
    expect(modelVendor('thinking-machines/x').name).toBe('Thinking Machines')
  })

  it('recognises well-known families without a vendor prefix', () => {
    expect(modelVendor('claude-sonnet-5-5').key).toBe('anthropic')
    expect(modelVendor('gpt-4o').key).toBe('openai')
    expect(modelVendor('gemini-3-pro').key).toBe('google')
    expect(modelVendor('something-else').key).toBe('other')
  })
})

describe('vendorTabs', () => {
  it('lists the biggest vendor first and counts its models', () => {
    const tabs = vendorTabs([
      option('google/a'),
      option('openai/a'),
      option('openai/b'),
      option('~openai/c'),
    ])
    expect(tabs.map((tab) => [tab.name, tab.count])).toEqual([
      ['OpenAI', 3],
      ['Google', 1],
    ])
  })

  it('folds the long tail into one More tab that holds every remaining model', () => {
    const options = Array.from({ length: MAX_VENDOR_TABS + 3 }, (_, index) =>
      option(`vendor${String(index).padStart(2, '0')}/model`)
    )
    const tabs = vendorTabs(options)
    expect(tabs).toHaveLength(MAX_VENDOR_TABS + 1)
    const more = tabs[tabs.length - 1]
    expect(more.key).toBe(OTHER_VENDORS_KEY)
    expect(more.count).toBe(3)
    expect(filterByVendorTab(options, OTHER_VENDORS_KEY, tabs)).toHaveLength(3)
  })
})

describe('filterByVendorTab', () => {
  const options = [option('google/a'), option('openai/a'), option('~openai/b')]
  const tabs = vendorTabs(options)

  it('keeps every model without a tab and one vendor with one', () => {
    expect(filterByVendorTab(options, null, tabs)).toHaveLength(3)
    expect(filterByVendorTab(options, 'openai', tabs).map((o) => o.id)).toEqual(
      ['openai/a', '~openai/b']
    )
  })
})
