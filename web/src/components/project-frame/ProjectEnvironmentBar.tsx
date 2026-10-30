// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { ChevronRight, ExternalLink, Plug, Rocket, Star } from 'lucide-react'
import { Link } from 'react-router'
import type { ProjectResponse } from '@/api/client'
import { Status } from '@temps-sdk/ds'
import { ExternalBadge } from '@/components/dashboard/ProjectCard'
import { ProjectServiceSwitcher } from '@/components/project-groups/ProjectServiceSwitcher'
import {
  ServiceGroupControl,
  useServiceGroup,
} from '@/components/project-groups/ServiceGroupControl'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
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
  environmentLiveStatus,
} from '@/lib/project-environment-selection'
import {
  describeDockerSocket,
  HOST_DOCKER_ACCESS_SHORT_LABEL,
} from '@/lib/docker-socket'
import { projectRoot } from '@/lib/project-navigation'
import { useProjectEnvironment } from './project-environment-context'

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

/**
 * The one place a project page's environment is chosen:
 * `project › environment  ● Live · Visit <env> ↗` plus the project actions.
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
  const environment = selection.environment
  const status = environment ? environmentLiveStatus(environment) : undefined
  const isDefault = !!environment && environment.id === defaultEnvironment?.id
  const hint = SCOPE_HINT[scope]
  const group = useServiceGroup(project.id).data?.group
  const external = isExternallyHosted(project)
  const dockerSocket = describeDockerSocket(project.docker_socket)

  return (
    // Same horizontal gutters as `PageContainer`, so the bar's left and right
    // edges line up with the page below it at every breakpoint.
    <header className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b px-4 py-2 sm:px-6 lg:px-8">
      <nav
        aria-label="Project and environment"
        className="flex min-w-0 items-center gap-1.5"
      >
        {/* The icon stays outside the heading: it has its own name, which
            would otherwise become part of the project's. */}
        <ServiceIcon service={project} size="sm" className="shrink-0" />
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
          className="size-4 shrink-0 text-muted-foreground"
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
        {status && !selection.all && (
          <Status tone={status.tone} label={status.label} variant="dot" />
        )}
      </nav>

      {hint && (
        <span className="text-xs text-muted-foreground max-md:hidden">
          {hint}
        </span>
      )}

      <div className="ml-auto flex items-center gap-2">
        {environment && !selection.all && !isDefault && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setDefault(environment.slug)}
            title={`Open ${environment.name} by default for this project`}
          >
            <Star className="size-4" aria-hidden="true" />
            <span className="max-sm:sr-only">Make default</span>
          </Button>
        )}
        {environment?.main_url && !selection.all && (
          <Button variant="outline" size="sm" asChild>
            <a
              href={environment.main_url}
              target="_blank"
              rel="noopener noreferrer"
              title={environment.main_url}
            >
              <ExternalLink className="size-4" aria-hidden="true" />
              <span className="max-sm:sr-only">Visit {environment.name}</span>
            </a>
          </Button>
        )}
        {/* Temps never deploys an app hosted elsewhere. */}
        {!external && (
          <Button size="sm" onClick={onDeploy}>
            <Rocket className="size-4" aria-hidden="true" />
            <span className="max-sm:sr-only">Deploy</span>
          </Button>
        )}
      </div>
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
        className="h-8 w-auto min-w-36 gap-2 font-medium"
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
