// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { defaultDeployEnvironment } from '@/lib/project-deploy-action'

/** URL search parameter that carries the selected environment's slug. */
export const ENVIRONMENT_PARAM = 'env'

/** Value of {@link ENVIRONMENT_PARAM} that selects every environment. */
export const ALL_ENVIRONMENTS = 'all'

/**
 * `?environment=` from links made before the project bar (alarms, Shield,
 * reachability, older bookmarks): an environment id, or sometimes a slug.
 */
export const LEGACY_ENVIRONMENT_PARAM = 'environment'

/** The slug a legacy `?environment=` value names, if it names one. */
export function legacyEnvironmentSlug(
  environments: ReadonlyArray<{ id: number; slug: string }>,
  value: string | null
): string | null {
  if (!value) return null
  const match = environments.find(
    (environment) =>
      String(environment.id) === value || environment.slug === value
  )
  return match?.slug ?? null
}

interface SelectableEnvironment {
  id: number
  slug: string
  is_preview: boolean
}

export type EnvironmentSelectionSource =
  /** `?env=` in the URL. */
  | 'url'
  /** The last environment picked for this project in this browser. */
  | 'remembered'
  /** The environment marked as this project's default. */
  | 'default'
  /** Production, else the first non-preview environment. */
  | 'fallback'

export interface EnvironmentSelection<T extends SelectableEnvironment> {
  /** The environment the page shows; undefined only when there is none. */
  environment: T | undefined
  /** True when the page shows every environment ("All environments"). */
  all: boolean
  source: EnvironmentSelectionSource
}

function bySlug<T extends SelectableEnvironment>(
  environments: readonly T[],
  slug: string | null | undefined
): T | undefined {
  return slug ? environments.find((env) => env.slug === slug) : undefined
}

/**
 * Which environment a project page shows.
 *
 * Order: the URL, then the environment last picked for this project, then the
 * project's default, then production. A slug that no longer exists (renamed or
 * deleted environment) is skipped rather than shown as an empty page.
 * "All environments" only applies where the page can list several; elsewhere
 * it falls through to the next candidate.
 */
export function resolveEnvironmentSelection<T extends SelectableEnvironment>({
  environments,
  urlParam,
  remembered,
  preferredDefault,
  allowAll,
}: {
  environments: readonly T[]
  urlParam: string | null
  remembered: string | null
  preferredDefault: string | null
  allowAll: boolean
}): EnvironmentSelection<T> {
  const fallback = defaultDeployEnvironment(environments)
  const candidates: [string | null, EnvironmentSelectionSource][] = [
    [urlParam, 'url'],
    [remembered, 'remembered'],
  ]
  for (const [value, source] of candidates) {
    if (value === ALL_ENVIRONMENTS && allowAll)
      return { environment: undefined, all: true, source }
    const match = bySlug(environments, value)
    if (match) return { environment: match, all: false, source }
  }
  const preferred = bySlug(environments, preferredDefault)
  if (preferred)
    return { environment: preferred, all: false, source: 'default' }
  return { environment: fallback, all: false, source: 'fallback' }
}

/** The environment new visitors see: the stored default, else production. */
export function resolveDefaultEnvironment<T extends SelectableEnvironment>(
  environments: readonly T[],
  preferredDefault: string | null
): T | undefined {
  return (
    bySlug(environments, preferredDefault) ??
    defaultDeployEnvironment(environments)
  )
}

export interface EnvironmentLiveStatus {
  tone: 'ok' | 'idle'
  label: string
}

/**
 * One-word state for the environment bar. Coarse on purpose: the overview
 * and the environment's own page explain a failed or degraded deployment.
 */
export function environmentLiveStatus(environment: {
  sleeping: boolean
  current_deployment_id?: number | null
}): EnvironmentLiveStatus {
  if (!environment.current_deployment_id)
    return { tone: 'idle', label: 'Not deployed' }
  if (environment.sleeping) return { tone: 'idle', label: 'Sleeping' }
  return { tone: 'ok', label: 'Live' }
}
