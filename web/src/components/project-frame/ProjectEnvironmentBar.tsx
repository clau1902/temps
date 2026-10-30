// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  ChevronRight,
  ExternalLink,
  MoreHorizontal,
  Plug,
  Rocket,
  Star,
} from 'lucide-react'
import { Link } from 'react-router'
import type { ProjectResponse } from '@/api/client'
import { Status } from '@temps-sdk/ds'
import { ExternalBadge } from '@/components/dashboard/ProjectCard'
import { ProjectServiceSwitcher } from '@/components/project-groups/ProjectServiceSwitcher'
import {
  ServiceGroupControl,
  useServiceGroup,
} from '@/components/project-groups/ServiceGroupControl'
import { ReachabilityStatus } from '@/components/reachability/ReachabilityStatus'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { Skeleton } from '@/components/ui/skeleton'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { ServiceIcon } from '@/components/project/ServiceIcon'
import {
  ALL_ENVIRONMENTS,
  type EnvironmentLiveStatus,
} from '@/lib/project-environment-selection'
import {
  describeDockerSocket,
  HOST_DOCKER_ACCESS_SHORT_LABEL,
} from '@/lib/docker-socket'
import { projectRoot } from '@/lib/project-navigation'
import { useProjectEnvironment } from './project-environment-context'
import { RepositoryProviderIcon } from './RepositoryProviderIcon'
import { useEnvironmentHealth } from './use-environment-health'
import { useProjectRepository } from './use-project-repository'

const SCOPE_HINT = {
  single: undefined,
  all: undefined,
  project: 'Applies to every environment',
} as const

/** Whether Temps never deploys this project: it runs somewhere else. */
function isExternallyHosted(project: ProjectResponse): boolean {
  // `hosting` is the explicit flag; `source_type: 'external'` marks a
  // monitoring-only project.
  return project.hosting === 'external' || project.source_type === 'external'
}

/** Where each problem state is explained, for its status to link to. */
function statusHref(
  projectSlug: string,
  status: EnvironmentLiveStatus,
  environmentSlug: string,
  latestDeploymentId: number | undefined
): string | undefined {
  const root = projectRoot(projectSlug)
  switch (status.explainedAt) {
    case 'deployment':
      return latestDeploymentId == null
        ? `${root}/deployments`
        : `${root}/deployments/${latestDeploymentId}`
    case 'environment':
      return `${root}/environments?env=${encodeURIComponent(environmentSlug)}`
    case 'monitors':
      return `${root}/monitors`
    case undefined:
      return undefined
  }
}

/**
 * The one place a project page's environment is chosen:
 * `project › environment  ● Live  Public at …` plus the project actions.
 * It is also the project's identity: its name is the page's top heading,
 * with the project (group) it belongs to and what it may do on its host.
 */
export function ProjectEnvironmentBar({
  project,
  onDeploy,
}: {
  project: ProjectResponse
  onDeploy: () => void
}) {
  const {
    environments,
    isPending,
    isError,
    retry,
    scope,
    selection,
    defaultEnvironment,
    setDefault,
  } = useProjectEnvironment()
  const environment = selection.all ? undefined : selection.environment
  const isDefault = !!environment && environment.id === defaultEnvironment?.id
  const hint = SCOPE_HINT[scope]
  const group = useServiceGroup(project.id).data?.group
  const external = isExternallyHosted(project)
  const dockerSocket = describeDockerSocket(project.docker_socket)
  const repository = useProjectRepository(project)
  // Temps runs nothing for an app hosted elsewhere: no deployments,
  // containers or address of its own to report on.
  const health = useEnvironmentHealth({
    project,
    environment,
    environments,
    enabled: !external,
    onDeploymentSettled: retry,
  })
  const status = health.status
  const href =
    status && environment
      ? statusHref(
          project.slug,
          status,
          environment.slug,
          health.latestDeploymentId
        )
      : undefined
  const showReachability =
    !!environment?.current_deployment_id && !external && !health.isWorker

  return (
    // Same horizontal gutters as `PageContainer`, so the bar's left and right
    // edges line up with the page below it at every breakpoint.
    <header className="flex shrink-0 items-center gap-x-3 gap-y-2 border-b px-4 py-2 sm:flex-wrap sm:px-6 lg:px-8">
      <nav
        aria-label="Project and environment"
        className="flex min-w-0 items-center gap-1.5"
      >
        {/* The icon stays outside the heading: it has its own name, which
            would otherwise become part of the project's. */}
        <ServiceIcon
          service={project}
          size="sm"
          className="shrink-0 max-sm:hidden"
        />
        <h1 className="min-w-0 truncate text-sm font-semibold">
          {group ? (
            <ProjectServiceSwitcher project={project} group={group}>
              {project.name}
            </ProjectServiceSwitcher>
          ) : (
            <Link
              to={`${projectRoot(project.slug)}/project`}
              className="block truncate rounded-md px-1 py-1 hover:bg-muted"
            >
              {project.name}
            </Link>
          )}
        </h1>
        <ServiceGroupControl project={project} />
        {external && <ExternalBadge />}
        {dockerSocket.state === 'granted' && (
          // Stays at every width: this is where the console states that the
          // project is root-equivalent on its host, so on a phone it shrinks
          // to a short label instead of disappearing.
          <Badge
            role="img"
            variant="outline"
            className="inline-flex shrink-0 gap-1"
            title={dockerSocket.detail}
            aria-label={`${dockerSocket.label}: ${dockerSocket.detail}`}
          >
            <Plug aria-hidden="true" className="size-3" />
            <span className="sm:hidden">{HOST_DOCKER_ACCESS_SHORT_LABEL}</span>
            <span className="hidden sm:inline">{dockerSocket.label}</span>
          </Badge>
        )}
        <ChevronRight
          aria-hidden="true"
          className="size-4 shrink-0 text-muted-foreground max-sm:hidden"
        />
        {isPending ? (
          <Skeleton className="h-8 w-36" />
        ) : isError ? (
          <Button variant="outline" size="sm" onClick={retry}>
            Environments failed to load · Retry
          </Button>
        ) : environments.length === 0 ? (
          <Link
            to={`${projectRoot(project.slug)}/environments`}
            className="text-sm text-muted-foreground underline underline-offset-4"
          >
            No environments yet
          </Link>
        ) : (
          <ProjectEnvironmentSelect />
        )}
        {status &&
          (href ? (
            // A problem links to where it is explained and fixed.
            <Link
              to={href}
              title={status.detail}
              aria-label={`${status.label}: ${status.detail}`}
              className="shrink-0 rounded-md px-1 py-0.5 hover:bg-muted"
            >
              <Status tone={status.tone} label={status.label} variant="dot" />
            </Link>
          ) : (
            <span
              title={status.detail}
              aria-label={`${status.label}: ${status.detail}`}
              role="img"
              className="shrink-0"
            >
              <Status tone={status.tone} label={status.label} variant="dot" />
            </span>
          ))}
      </nav>

      {showReachability && environment && (
        // Per environment, per address (ADR 064 D3): what a visitor can
        // open, or why not and the button that fixes it. The overview and
        // Domains say the same on a phone, where the bar keeps to one row.
        <div className="min-w-0 max-md:hidden">
          <ReachabilityStatus
            projectId={project.id}
            projectSlug={project.slug}
            environmentId={environment.id}
          />
        </div>
      )}

      {hint && (
        <span className="text-xs text-muted-foreground max-md:hidden">
          {hint}
        </span>
      )}

      <div className="ml-auto flex items-center gap-2 max-sm:hidden">
        {environment && !isDefault && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setDefault(environment.slug)}
            title={`Open ${environment.name} by default for this project`}
          >
            <Star className="size-4" aria-hidden="true" />
            Make default
          </Button>
        )}
        {repository && (
          <Button variant="outline" size="icon" className="size-9" asChild>
            <a
              href={repository.url}
              target="_blank"
              rel="noopener noreferrer"
              aria-label="Open repository in a new window"
              title={repository.url}
            >
              <RepositoryProviderIcon
                provider={repository.provider}
                className="size-4"
              />
            </a>
          </Button>
        )}
        {environment?.main_url && (
          <Button variant="outline" size="sm" asChild>
            <a
              href={environment.main_url}
              target="_blank"
              rel="noopener noreferrer"
              title={environment.main_url}
            >
              <ExternalLink className="size-4" aria-hidden="true" />
              Visit {environment.name}
            </a>
          </Button>
        )}
        {/* Temps never deploys an app hosted elsewhere. */}
        {!external && (
          <Button size="sm" onClick={onDeploy}>
            <Rocket className="size-4" aria-hidden="true" />
            Deploy
          </Button>
        )}
      </div>

      {/* On a phone the actions share one menu, so the project, its
          environment and its state keep to a single row. */}
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            variant="outline"
            size="icon"
            className="ml-auto size-8 sm:hidden"
            aria-label="Project actions"
          >
            <MoreHorizontal className="size-4" aria-hidden="true" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          {!external && (
            <DropdownMenuItem onSelect={onDeploy}>
              <Rocket className="size-4" aria-hidden="true" />
              Deploy
            </DropdownMenuItem>
          )}
          {environment?.main_url && (
            <DropdownMenuItem asChild>
              <a
                href={environment.main_url}
                target="_blank"
                rel="noopener noreferrer"
              >
                <ExternalLink className="size-4" aria-hidden="true" />
                Visit {environment.name}
              </a>
            </DropdownMenuItem>
          )}
          {repository && (
            <DropdownMenuItem asChild>
              <a
                href={repository.url}
                target="_blank"
                rel="noopener noreferrer"
              >
                <RepositoryProviderIcon
                  provider={repository.provider}
                  className="size-4"
                />
                Open repository
              </a>
            </DropdownMenuItem>
          )}
          {environment && !isDefault && (
            <DropdownMenuItem onSelect={() => setDefault(environment.slug)}>
              <Star className="size-4" aria-hidden="true" />
              Make {environment.name} the default
            </DropdownMenuItem>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </header>
  )
}

/**
 * The environment picker of the project bar, on its own: monitoring-only
 * projects show it in their header, without the bar's deploy actions.
 */
export function ProjectEnvironmentSelect() {
  const { environments, scope, selection, select, defaultEnvironment } =
    useProjectEnvironment()
  const production = environments.filter((env) => !env.is_preview)
  const previews = environments.filter((env) => env.is_preview)
  const selectValue = selection.all
    ? ALL_ENVIRONMENTS
    : (selection.environment?.slug ?? undefined)
  return (
    <Select value={selectValue} onValueChange={select}>
      <SelectTrigger
        aria-label="Environment"
        className="h-8 w-auto min-w-0 gap-2 font-medium sm:min-w-36"
      >
        <SelectValue placeholder="Select environment" />
      </SelectTrigger>
      <SelectContent>
        {scope === 'all' && (
          <>
            <SelectItem value={ALL_ENVIRONMENTS}>All environments</SelectItem>
            <SelectSeparator />
          </>
        )}
        <SelectGroup>
          {previews.length > 0 && <SelectLabel>Environments</SelectLabel>}
          {production.map((env) => (
            <SelectItem key={env.id} value={env.slug}>
              {env.name}
              {env.id === defaultEnvironment?.id ? ' (default)' : ''}
            </SelectItem>
          ))}
        </SelectGroup>
        {previews.length > 0 && (
          <SelectGroup>
            <SelectSeparator />
            <SelectLabel>Previews</SelectLabel>
            {previews.map((env) => (
              <SelectItem key={env.id} value={env.slug}>
                {env.name}
              </SelectItem>
            ))}
          </SelectGroup>
        )}
      </SelectContent>
    </Select>
  )
}
