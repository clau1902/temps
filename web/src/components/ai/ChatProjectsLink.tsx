// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  getApplicationOptions,
  getLastDeploymentOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { ProjectAvatar } from '@/components/project/ProjectAvatar'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { useQuery } from '@tanstack/react-query'
import { ChevronDown, ExternalLink, FolderGit2 } from 'lucide-react'
import { Link } from 'react-router'
import { useChatBlueprint } from './chat-blueprint'
import { chatProjects, projectHref } from './chat-projects'
import { useChatResources } from './chat-resources'
import type { ProjectRef } from './project-refs'

/**
 * The Temps project this chat builds, one click away: its page, and the live
 * app once a deployment has finished. A chat working on several projects
 * lists them. A chat that has not made or joined a project shows nothing,
 * since there is nothing to open yet.
 */
export function ChatProjectsLink({
  conversationPublicId,
}: {
  conversationPublicId: string
}) {
  const { applicationId } = useChatBlueprint(conversationPublicId)
  const application = useQuery({
    ...getApplicationOptions({
      path: { application_public_id: applicationId ?? '' },
    }),
    enabled: applicationId !== null,
  })
  const resources = useChatResources(conversationPublicId)
  const projects = chatProjects(
    application.data?.projects ?? [],
    resources.data?.resources ?? []
  )

  if (projects.length === 0) return null
  if (projects.length === 1) return <SingleProjectLink project={projects[0]} />
  return <ProjectsMenu projects={projects} />
}

function SingleProjectLink({ project }: { project: ProjectRef }) {
  const latest = useQuery({
    ...getLastDeploymentOptions({ path: { id: project.id } }),
    // A project that never deployed answers with an error; that is a state,
    // not something to retry.
    retry: false,
    staleTime: 30_000,
  })
  const liveUrl =
    latest.data?.status === 'completed' ? (latest.data.url ?? null) : null
  return (
    <div className="flex items-center">
      <Button
        asChild
        variant="outline"
        size="sm"
        className="h-8 max-w-56 gap-1.5 rounded-r-none"
      >
        <Link
          to={projectHref(project)}
          title={`Open the project ${project.name}`}
        >
          <ProjectAvatar
            name={project.name}
            className="size-4 shrink-0 rounded-sm"
            fallbackClassName="rounded-sm bg-primary/10 text-[10px] text-primary"
          />
          <span className="hidden min-w-0 truncate sm:inline">
            <span className="text-muted-foreground">Project </span>
            {project.name}
          </span>
        </Link>
      </Button>
      {liveUrl ? (
        <Button
          asChild
          variant="outline"
          size="sm"
          className="h-8 rounded-l-none border-l-0 px-2"
        >
          <a
            href={liveUrl}
            target="_blank"
            rel="noopener noreferrer"
            title={`Open the live app: ${liveUrl}`}
            aria-label={`Open the live ${project.name} app`}
          >
            <ExternalLink className="size-3.5" />
          </a>
        </Button>
      ) : (
        <span
          className="flex h-8 items-center rounded-r-md border border-l-0 px-2 text-xs text-muted-foreground"
          title={
            latest.isPending
              ? 'Checking the latest deployment'
              : 'No finished deployment yet, so there is no live app to open'
          }
        >
          <ExternalLink className="size-3.5 opacity-40" aria-hidden="true" />
        </span>
      )}
    </div>
  )
}

function ProjectsMenu({ projects }: { projects: readonly ProjectRef[] }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="h-8 gap-1.5"
          title="Open one of the projects this chat works on"
        >
          <FolderGit2 className="size-4 shrink-0" />
          <span className="hidden sm:inline">{projects.length} projects</span>
          <ChevronDown className="size-3.5 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-60">
        <DropdownMenuLabel className="text-xs text-muted-foreground">
          Projects this chat works on
        </DropdownMenuLabel>
        {projects.map((project) => (
          <DropdownMenuItem key={project.id} asChild>
            <Link to={projectHref(project)} className="gap-2">
              <ProjectAvatar
                name={project.name}
                className="size-4 shrink-0 rounded-sm"
                fallbackClassName="rounded-sm bg-primary/10 text-[10px] text-primary"
              />
              <span className="min-w-0 flex-1 truncate">{project.name}</span>
              <span className="shrink-0 font-mono text-xs text-muted-foreground">
                {project.slug}
              </span>
            </Link>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}
