// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { ArrowRight } from 'lucide-react'
import { Link } from 'react-router'
import type {
  BuildLocation,
  EnvironmentResponse,
  ProjectResponse,
} from '@/api/client'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { CombinedProjectSettings } from '@/components/project/settings/CombinedProjectSettings'
import { ENVIRONMENT_PARAM } from '@/lib/project-environment-selection'
import { projectRoot } from '@/lib/project-navigation'
import { useProjectEnvironment } from './project-environment-context'

const BUILD_LOCATION_LABEL: Record<BuildLocation, string> = {
  control_plane: 'Control plane',
  node: 'Worker node',
  ci: 'CI',
}

function formatCpu(cores: number | null | undefined) {
  return cores ? `${cores} ${cores === 1 ? 'core' : 'cores'}` : 'No limit'
}

function formatMemory(mb: number | null | undefined) {
  if (!mb) return 'No limit'
  return mb >= 1024 && mb % 1024 === 0 ? `${mb / 1024} GB` : `${mb} MB`
}

function environmentFacts(
  project: ProjectResponse,
  environment: EnvironmentResponse
) {
  const config = environment.deployment_config
  return [
    {
      label: 'Branch',
      value: environment.branch || project.main_branch || 'Not set',
      mono: true,
    },
    {
      label: 'Auto-deploy',
      value:
        (config?.automaticDeploy ?? project.deployment_config.automaticDeploy)
          ? 'On push'
          : 'Manual',
    },
    { label: 'Replicas', value: String(config?.replicas ?? 1) },
    { label: 'CPU', value: formatCpu(config?.cpuLimit) },
    { label: 'Memory', value: formatMemory(config?.memoryLimit) },
    {
      label: 'Builds on',
      value: config?.buildLocation
        ? BUILD_LOCATION_LABEL[config.buildLocation]
        : 'Control plane',
    },
  ]
}

/**
 * What the selected environment actually runs, above the project-wide build
 * settings. Branch, replicas and resources are per environment, so they are
 * edited on the environment; this summary is where someone looking for them
 * under "Build & deploy" finds them.
 */
function EnvironmentRuntimeSummary({ project }: { project: ProjectResponse }) {
  const { selection, isPending } = useProjectEnvironment()
  const environment = selection.environment
  if (isPending)
    return (
      <section aria-busy="true">
        <Skeleton className="h-24 w-full" />
      </section>
    )
  if (!environment) return null
  const settingsHref = `${projectRoot(project.slug)}/environments?view=settings&${ENVIRONMENT_PARAM}=${encodeURIComponent(environment.slug)}`
  // Laid out like every other group on the page: what it is on the left,
  // the values on the right as label/value rows.
  return (
    <section
      aria-labelledby="environment-runtime-heading"
      className="grid min-w-0 gap-5 md:grid-cols-[minmax(0,1fr)_minmax(0,2fr)] md:gap-10"
    >
      <div className="min-w-0">
        <h2
          id="environment-runtime-heading"
          className="text-base font-semibold"
        >
          How {environment.name} deploys
        </h2>
        <p className="mt-1 text-sm text-muted-foreground">
          Set per environment. Everything below applies to every environment.
        </p>
      </div>
      <div className="min-w-0 space-y-4">
        <dl className="divide-y">
          {environmentFacts(project, environment).map((fact) => (
            <div
              key={fact.label}
              className="flex items-baseline justify-between gap-6 py-2 first:pt-0"
            >
              <dt className="text-sm text-muted-foreground">{fact.label}</dt>
              <dd
                className={
                  fact.mono
                    ? 'min-w-0 truncate font-mono text-sm'
                    : 'min-w-0 truncate text-sm font-medium'
                }
              >
                {fact.value}
              </dd>
            </div>
          ))}
        </dl>
        <Button asChild variant="outline" size="sm">
          <Link to={settingsHref}>
            Edit {environment.name} settings
            <ArrowRight className="size-4" aria-hidden="true" />
          </Link>
        </Button>
      </div>
    </section>
  )
}

/** Build & deploy: the environment's runtime, then project-wide delivery. */
export function BuildAndDeployPage({
  project,
  refetch,
}: {
  project: ProjectResponse
  refetch: () => void
}) {
  return (
    <CombinedProjectSettings
      page="delivery"
      project={project}
      refetch={refetch}
      intro={<EnvironmentRuntimeSummary project={project} />}
    />
  )
}
