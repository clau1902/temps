// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AssistantModelOption } from './assistant-models'

/** Who makes a model, as far as its id says. */
export interface ModelVendor {
  /** Stable key: the lowercase vendor slug, `other` when the id names none. */
  key: string
  /** Name to show. */
  name: string
  /** File in `public/ai-agents/`, when the repository ships that vendor's mark. */
  logo: string | null
}

interface KnownVendor {
  name: string
  logo?: string
}

/**
 * Vendor slugs as they appear before the `/` in a model id. Logos reuse the
 * marks already shipped for the AI-agent analytics; a vendor without one
 * renders as a lettered chip instead of a missing image.
 */
const KNOWN_VENDORS: Record<string, KnownVendor> = {
  openai: { name: 'OpenAI', logo: 'openai.svg' },
  anthropic: { name: 'Anthropic', logo: 'anthropic.svg' },
  google: { name: 'Google', logo: 'google.svg' },
  mistralai: { name: 'Mistral', logo: 'mistral.svg' },
  deepseek: { name: 'DeepSeek', logo: 'deepseek.svg' },
  'x-ai': { name: 'xAI', logo: 'xai.svg' },
  meta: { name: 'Meta', logo: 'meta.svg' },
  'meta-llama': { name: 'Meta', logo: 'meta.svg' },
  cohere: { name: 'Cohere', logo: 'cohere.svg' },
  amazon: { name: 'Amazon', logo: 'amazon.svg' },
  perplexity: { name: 'Perplexity', logo: 'perplexity.svg' },
  bytedance: { name: 'ByteDance', logo: 'bytedance.svg' },
  'bytedance-seed': { name: 'ByteDance', logo: 'bytedance.svg' },
  openrouter: { name: 'OpenRouter', logo: 'openrouter.svg' },
  apple: { name: 'Apple', logo: 'apple.svg' },
  qwen: { name: 'Qwen' },
  'z-ai': { name: 'Z.ai' },
  nvidia: { name: 'NVIDIA' },
  moonshotai: { name: 'Moonshot AI' },
  minimax: { name: 'MiniMax' },
  microsoft: { name: 'Microsoft' },
  tencent: { name: 'Tencent' },
  xiaomi: { name: 'Xiaomi' },
}

/** Name of the vendor given to ids that name none. */
export const OTHER_VENDOR_NAME = 'Other'

const OTHER: ModelVendor = { key: 'other', name: OTHER_VENDOR_NAME, logo: null }

/** Ids with no `vendor/` prefix still name their family. */
function vendorFromBareId(id: string): string | null {
  const lower = id.toLowerCase()
  if (lower.startsWith('claude')) return 'anthropic'
  if (lower.startsWith('gemini') || lower.startsWith('gemma')) return 'google'
  if (/^(gpt|chatgpt|o\d)/.test(lower)) return 'openai'
  if (lower.startsWith('deepseek')) return 'deepseek'
  if (lower.startsWith('grok')) return 'x-ai'
  if (lower.startsWith('mistral') || lower.startsWith('codestral')) {
    return 'mistralai'
  }
  return null
}

function titleCase(slug: string): string {
  return slug
    .split(/[-_.]/)
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

/**
 * The vendor of a model id: `google/gemini-3.5-flash` is Google. A leading `~`
 * (a router alias such as `~openai/gpt-latest`) belongs to the same vendor.
 */
export function modelVendor(modelId: string): ModelVendor {
  const id = modelId.trim().replace(/^~/, '')
  const slash = id.indexOf('/')
  const slug = (slash > 0 ? id.slice(0, slash) : vendorFromBareId(id))
    ?.toLowerCase()
    .trim()
  if (!slug) return OTHER
  const known = KNOWN_VENDORS[slug]
  if (known) {
    // Two slugs can share a vendor (`meta` and `meta-llama`): key by name.
    return {
      key: known.name.toLowerCase().replace(/[^a-z0-9]+/g, '-'),
      name: known.name,
      logo: known.logo ?? null,
    }
  }
  return { key: slug, name: titleCase(slug) || slug, logo: null }
}

/** A vendor tab: who, and how many of the listed models are theirs. */
export interface VendorTab {
  key: string
  name: string
  logo: string | null
  count: number
}

/** Key of the tab that holds every vendor not given a tab of its own. */
export const OTHER_VENDORS_KEY = '__others'

/** Vendor tabs shown before the rest are folded into one. */
export const MAX_VENDOR_TABS = 8

/**
 * Tabs for the picker, biggest vendor first. Past [`MAX_VENDOR_TABS`] the long
 * tail folds into one "More" tab so the row stays scannable; search still finds
 * any model in every tab.
 */
export function vendorTabs(
  options: readonly AssistantModelOption[]
): VendorTab[] {
  const byKey = new Map<string, VendorTab>()
  for (const option of options) {
    const vendor = modelVendor(option.id)
    const tab = byKey.get(vendor.key)
    if (tab) tab.count += 1
    else byKey.set(vendor.key, { ...vendor, count: 1 })
  }
  const sorted = [...byKey.values()].sort(
    (left, right) =>
      right.count - left.count || left.name.localeCompare(right.name)
  )
  if (sorted.length <= MAX_VENDOR_TABS) return sorted
  const shown = sorted.slice(0, MAX_VENDOR_TABS)
  const rest = sorted.slice(MAX_VENDOR_TABS)
  return [
    ...shown,
    {
      key: OTHER_VENDORS_KEY,
      name: 'More',
      logo: null,
      count: rest.reduce((total, tab) => total + tab.count, 0),
    },
  ]
}

/** The options of one tab; every option when `tabKey` is `null`. */
export function filterByVendorTab(
  options: readonly AssistantModelOption[],
  tabKey: string | null,
  tabs: readonly VendorTab[]
): AssistantModelOption[] {
  if (tabKey === null) return [...options]
  if (tabKey === OTHER_VENDORS_KEY) {
    const shown = new Set(
      tabs.filter((tab) => tab.key !== OTHER_VENDORS_KEY).map((tab) => tab.key)
    )
    return options.filter((option) => !shown.has(modelVendor(option.id).key))
  }
  return options.filter((option) => modelVendor(option.id).key === tabKey)
}
