// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/** The slice of the usage response the spend readout needs. */
export interface ChatSpendInput {
  requests: number
  cost_microcents: number
  unpriced_requests: number
}

/** One microcent is 1/10000 of a cent, so a dollar is 1,000,000 of them. */
const MICROCENTS_PER_DOLLAR = 1_000_000

/**
 * Dollars for an estimated cost. Chats cost fractions of a cent, so small
 * amounts keep four decimals instead of rounding to a misleading `$0.00`.
 */
export function formatSpend(microcents: number): string {
  const dollars = Math.max(0, microcents) / MICROCENTS_PER_DOLLAR
  if (dollars === 0) return '$0.00'
  if (dollars < 0.0001) return '<$0.0001'
  if (dollars < 1) return `$${dollars.toFixed(4)}`
  return `$${dollars.toFixed(2)}`
}

/**
 * The headline for a chat's spend. A chat that used a model with no known
 * price is shown as a floor ("at least"), never as a complete total, and a
 * chat whose every request was unpriced says the cost is unknown.
 */
export function spendLabel(usage: ChatSpendInput): string {
  if (usage.requests === 0) return 'No spend yet'
  if (usage.unpriced_requests >= usage.requests) return 'Cost unknown'
  const amount = formatSpend(usage.cost_microcents)
  return usage.unpriced_requests > 0 ? `At least ${amount}` : amount
}

/** Compact token count: 1,234 → 1.2k, 2,500,000 → 2.5M. */
export function formatTokens(tokens: number): string {
  if (tokens < 1000) return String(tokens)
  if (tokens < 1_000_000) return `${(tokens / 1000).toFixed(1)}k`
  return `${(tokens / 1_000_000).toFixed(1)}M`
}
