// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// What the console says about whether a project's addresses reach their
// audience (ADR 064 D3), and the small decisions the Make it reachable
// dialog makes from a plan and a run (D4).

import type {
  AddressReachability,
  EnvironmentReachability,
  PlanOption,
  ReachabilityPlan,
  RunState,
  RunView,
  SetupField,
} from '@/api/client'

export type ReachabilityHeadline =
  | { kind: 'public'; address: string; url: string }
  | { kind: 'not_reachable'; reason: string; canFix: boolean }
  | { kind: 'unknown'; reason: string }
  | { kind: 'checking'; reason: string }

/** The address a visitor opens, as a URL. */
export function addressUrl(address: string): string {
  return /^https?:\/\//.test(address) ? address : `https://${address}`
}

/** The best address to show: reachable custom domains before generated ones. */
export function bestAddress(
  addresses: AddressReachability[]
): AddressReachability | undefined {
  const reachable = addresses.filter((a) => a.state === 'reachable')
  return (
    reachable.find((a) => a.source === 'custom_domain') ??
    reachable.find((a) => a.source === 'environment_domain') ??
    reachable[0]
  )
}

/** The header line: "Public at …" or "Not reachable yet — <reason>". */
export function reachabilityHeadline(
  reachability: EnvironmentReachability
): ReachabilityHeadline {
  const best = bestAddress(reachability.addresses)
  if (best)
    return {
      kind: 'public',
      address: best.address,
      url: addressUrl(best.address),
    }
  if (reachability.state === 'checking')
    return { kind: 'checking', reason: reachability.summary }
  if (reachability.state === 'unknown')
    return { kind: 'unknown', reason: reachability.summary }
  return {
    kind: 'not_reachable',
    reason: reachability.summary,
    canFix: (reachability.plan?.options.length ?? 0) > 0,
  }
}

/** How traffic gets in, in words. */
export function wayInLabel(wayIn: string | null | undefined): string {
  if (!wayIn) return 'No way in yet'
  if (wayIn === 'direct') return 'Direct'
  if (wayIn === 'tunnel:new') return 'A new tunnel'
  const [kind, name] = wayIn.split(':', 2)
  if (kind === 'tunnel') return `Tunnel ${name}`
  if (kind === 'direct')
    return name === 'control-plane' ? 'This server' : `Straight to ${name}`
  return wayIn
}

export const AUDIENCE_LABELS: Record<AddressReachability['audience'], string> =
  {
    public: 'Anyone',
    private: 'My network',
    signed_in: 'Signed-in people',
  }

/**
 * The run is still doing something (or Temps is recovering it after a
 * restart); keep polling it.
 */
export function isActiveRun(state: RunState): boolean {
  return state === 'running' || state === 'undoing' || state === 'recovering'
}

/**
 * Whether the run left something that undo would remove. `created` is the
 * journal's done entries; undo marks what it removes, so it shrinks.
 */
export function canUndoRun(run: RunView): boolean {
  if (isActiveRun(run.state) || run.state === 'undone') return false
  if (run.journal?.some((e) => e.state === 'pending')) return false
  return run.created.length > 0
}

/** A waiting or interrupted run carries on from its first unfinished step. */
export function canResumeRun(run: RunView): boolean {
  if (run.state !== 'waiting' && run.state !== 'interrupted') return false
  return !run.journal?.some((e) => e.state === 'pending')
}

/**
 * Whether resuming needs the Cloudflare token again: the run had not saved
 * the account yet, and the token is never stored before that.
 */
export function resumeNeedsToken(run: RunView): boolean {
  return run.steps.some(
    (s) =>
      s.kind === 'save_dns_provider' &&
      s.status !== 'done' &&
      s.status !== 'skipped'
  )
}

/**
 * The new plan a refused apply carries (409, ADR 064 D4): the cluster
 * changed since the plan was made, so nothing was created and this is the
 * plan to review instead.
 */
export function stalePlan(error: unknown): ReachabilityPlan | null {
  if (!error || typeof error !== 'object') return null
  const plan = (error as { plan?: unknown }).plan
  if (!plan || typeof plan !== 'object') return null
  return Array.isArray((plan as { options?: unknown }).options)
    ? (plan as ReachabilityPlan)
    : null
}

/** The option to preselect: the recommended one, else the first. */
export function defaultOption(options: PlanOption[]): PlanOption | undefined {
  return options.find((o) => o.recommended) ?? options[0]
}

/**
 * Answers for the apply request, from the form's text values. DNS provider
 * fields carry the provider id as a number; blank answers are left out.
 */
export function applyValues(
  fields: SetupField[],
  answers: Record<string, string>,
  address: string | null | undefined
): Record<string, string | number> {
  const values: Record<string, string | number> = {}
  const asked = new Set(fields.map((f) => f.key))
  if (!asked.has('address') && address) values.address = address
  for (const field of fields) {
    const answer = answers[field.key]?.trim()
    if (!answer) continue
    values[field.key] =
      field.kind.type === 'dns_provider' && /^\d+$/.test(answer)
        ? Number(answer)
        : answer
  }
  return values
}

/** The required fields still unanswered, by label. */
export function missingAnswers(
  fields: SetupField[],
  answers: Record<string, string>
): string[] {
  return fields
    .filter((f) => f.required && !answers[f.key]?.trim())
    .map((f) => f.label)
}

export interface CertificateChoices {
  /** HTTP-01 is listed: something answers on port 80, or Temps cannot tell. */
  showHttp01: boolean
  /** The choice labelled Recommended, if any. */
  recommended: 'tunnel' | 'http-01' | 'dns-01' | null
  /** Why HTTP-01 is not listed. */
  http01Note: string | null
}

/**
 * Which certificate methods Add Domain offers. HTTP-01 is only offered, and
 * only recommended, when a direct ingress takes port 80 (ADR 064 D4). While
 * Temps cannot tell it stays listed without the label.
 */
export function certificateChoices({
  coveringTunnels,
  wildcard,
  http01Available,
}: {
  coveringTunnels: number
  wildcard: boolean
  http01Available: boolean | undefined
}): CertificateChoices {
  const showHttp01 = !wildcard && http01Available !== false
  const recommended =
    coveringTunnels > 0
      ? 'tunnel'
      : showHttp01 && http01Available === true
        ? 'http-01'
        : http01Available === false || wildcard
          ? 'dns-01'
          : null
  return {
    showHttp01,
    recommended,
    http01Note:
      !wildcard && http01Available === false
        ? 'HTTP-01 is not offered: nothing in this cluster answers on port 80, which it needs. Use DNS-01, or a tunnel.'
        : null,
  }
}
