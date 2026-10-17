// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  getDeploymentJobsOptions,
  getDeploymentOptions,
  getLastDeploymentOptions,
  getProjectOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { ElapsedTime } from '@/components/global/ElapsedTime'
import { Button } from '@/components/ui/button'
import { CopyButton } from '@/components/ui/copy-button'
import { Skeleton } from '@/components/ui/skeleton'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { TimeAgo } from '@/components/utils/TimeAgo'
import { cn } from '@/lib/utils'
import { useQuery } from '@tanstack/react-query'
import {
  ArrowRight,
  ExternalLink,
  FolderGit2,
  GitBranch,
  Layers,
  Monitor,
  Rocket,
  X,
} from 'lucide-react'
import { useEffect, useState } from 'react'
import { Link } from 'react-router'
import { ChatPreviewTab } from './ChatPreviewTab'
import { ChatResourcesTab } from './ChatResourcesTab'
import { ProjectTab } from './ProjectTab'
import type { ProjectRef } from './project-refs'
import {
  isActiveDeploymentStatus,
  isFailedDeploymentStatus,
} from './deployment-card-state'
import { JobStatusIcon, statusPresentation } from './GeneratedDeploymentCard'
import type { WatchedDeployment } from './goal-presentation'

const MAX_TABS = 6

/** The fixed tab that lists what the chat made. */
const MADE_TAB = 'made'

/** The fixed tab that shows the page the chat's sandbox is serving. */
export const PREVIEW_TAB = 'preview'

/** A tab beside the chat: a deployment it watches or a project the user opened. */
export type SidebarTab =
  | { kind: 'deployment'; deployment: WatchedDeployment }
  | { kind: 'project'; project: ProjectRef }

export function tabKey(tab: SidebarTab): string {
  return tab.kind === 'project'
    ? `project:${tab.project.id}`
    : key(tab.deployment)
}

/** How far along the build is, as whole stages finished out of all of them. */
export function stageProgress(jobs: ReadonlyArray<{ status: string }>): {
  done: number
  total: number
} {
  return {
    done: jobs.filter((job) => job.status === 'success').length,
    total: jobs.length,
  }
}

function DeploymentTab({
  projectId,
  deploymentId,
}: {
  projectId: number
  deploymentId: number | null
}) {
  // A goal that watches "the latest deployment" follows whichever one that is.
  const poll = (query: { state: { data?: { status: string } } }) =>
    query.state.data && !isActiveDeploymentStatus(query.state.data.status)
      ? false
      : 3000
  const pinned = useQuery({
    ...getDeploymentOptions({
      path: { project_id: projectId, deployment_id: deploymentId ?? 0 },
    }),
    enabled: deploymentId !== null,
    refetchInterval: poll,
  })
  const latest = useQuery({
    ...getLastDeploymentOptions({ path: { id: projectId } }),
    enabled: deploymentId === null,
    refetchInterval: poll,
  })
  const deployment = deploymentId === null ? latest : pinned
  const active =
    deployment.data !== undefined &&
    isActiveDeploymentStatus(deployment.data.status)
  const jobs = useQuery({
    ...getDeploymentJobsOptions({
      path: {
        project_id: projectId,
        deployment_id: deployment.data?.id ?? 0,
      },
    }),
    enabled: deployment.data !== undefined,
    // One last fetch happens when the deployment stops being active, so the
    // final stage statuses are never left half-updated.
    refetchInterval: active ? 1500 : false,
  })
  const project = useQuery(getProjectOptions({ path: { id: projectId } }))

  if (deployment.isPending) {
    return (
      <div className="space-y-3 p-4">
        <Skeleton className="h-5 w-40" />
        <Skeleton className="h-4 w-56" />
        <Skeleton className="h-24 w-full" />
      </div>
    )
  }
  if (deployment.isError || !deployment.data) {
    return (
      <p className="p-4 text-sm text-muted-foreground">
        Temps could not load this deployment. It may have been removed, or you
        may not have access to it.
      </p>
    )
  }

  const data = deployment.data
  const status = statusPresentation(data.status)
  const stages = jobs.data?.jobs ?? []
  const progress = stageProgress(stages)
  const slug = project.data?.slug
  const live = data.status === 'completed' && data.url
  return (
    <div className="flex min-h-0 flex-col" aria-live="polite">
      <div className="space-y-2 px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-semibold">
            {project.data?.name ?? 'Deployment'} · #{data.id}
          </h3>
          <span className={cn('text-xs font-medium', status.className)}>
            {status.label}
          </span>
        </div>
        <p className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
          <span>{data.environment.name}</span>
          {data.branch && (
            <span className="inline-flex min-w-0 items-center gap-1 font-mono">
              <GitBranch className="size-3 shrink-0" />
              <span className="truncate">{data.branch}</span>
              {data.commit_hash && (
                <span>@ {data.commit_hash.slice(0, 7)}</span>
              )}
            </span>
          )}
          <span>
            started <TimeAgo date={data.started_at ?? data.created_at} />
          </span>
        </p>
        {data.url && (
          <div className="flex min-w-0 items-center gap-1.5">
            {live ? (
              <a
                href={data.url}
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex min-w-0 items-start gap-1 text-sm font-medium text-primary underline-offset-2 hover:underline"
              >
                <span className="break-all">{data.url}</span>
                <ExternalLink className="mt-0.5 size-3.5 shrink-0" />
              </a>
            ) : (
              <span
                className="break-all text-sm text-muted-foreground"
                title="The link opens once the deployment is live"
              >
                {data.url}
                <span className="ml-1 text-xs">(once live)</span>
              </span>
            )}
            <CopyButton value={data.url} className="size-6 shrink-0" />
          </div>
        )}
        {data.failure && (
          <p className="rounded-md border border-destructive/30 bg-destructive/5 px-2.5 py-2 text-xs text-destructive">
            {data.failure.title}
          </p>
        )}
      </div>

      <div className="border-t px-4 py-3">
        <div className="mb-2 flex items-center justify-between text-xs">
          <span className="font-medium">Stages</span>
          {progress.total > 0 && (
            <span className="text-muted-foreground">
              {progress.done} of {progress.total}
            </span>
          )}
        </div>
        {progress.total > 0 && (
          <div
            role="progressbar"
            aria-label="Deployment stages finished"
            aria-valuemin={0}
            aria-valuemax={progress.total}
            aria-valuenow={progress.done}
            className="mb-3 h-1 overflow-hidden rounded-full bg-muted"
          >
            <div
              className={cn(
                'h-full rounded-full transition-[width]',
                isFailedDeploymentStatus(data.status)
                  ? 'bg-destructive'
                  : 'bg-primary'
              )}
              style={{ width: `${(progress.done / progress.total) * 100}%` }}
            />
          </div>
        )}
        {jobs.isPending ? (
          <Skeleton className="h-20 w-full" />
        ) : stages.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            The stages appear here as soon as the build starts.
          </p>
        ) : (
          <ol className="grid gap-2">
            {stages.map((job) => (
              <li
                key={job.id}
                className="flex min-w-0 items-center gap-2 text-sm"
              >
                <JobStatusIcon status={job.status} />
                <span className="min-w-0 flex-1 truncate">{job.name}</span>
                {job.started_at ? (
                  <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                    <ElapsedTime
                      startedAt={job.started_at}
                      endedAt={job.finished_at}
                    />
                  </span>
                ) : (
                  <span className="shrink-0 text-xs capitalize text-muted-foreground">
                    {job.status}
                  </span>
                )}
              </li>
            ))}
          </ol>
        )}
      </div>

      {slug && (
        <div className="border-t px-4 py-3">
          <Button asChild size="sm" variant="outline" className="h-8">
            <Link to={`/projects/${slug}/deployments/${data.id}`}>
              View logs and details
              <ArrowRight aria-hidden="true" />
            </Link>
          </Button>
        </div>
      )}
    </div>
  )
}

/**
 * What this chat is working on, one tab each: the deployments it has started
 * or watched, with their build stages, and the projects the user opened from a
 * reply. Sits beside the conversation so progress stays visible while the
 * assistant keeps working.
 */
export function DeploymentSidebar({
  deployments,
  projects = [],
  focus,
  conversationPublicId,
  onOpenProject,
  onClose,
  inDrawer = false,
}: {
  deployments: WatchedDeployment[]
  /** Projects opened from the chat, newest first. */
  projects?: ProjectRef[]
  /** Selects a tab each time `nonce` changes, even if it is already open. */
  focus?: { key: string; nonce: number }
  /** The chat, for the tab listing what it made. Without it that tab is left out. */
  conversationPublicId?: string
  /** Opens a project from the "made" tab as a tab of its own. */
  onOpenProject?: (project: ProjectRef) => void
  /** Omit when the panel is a fixed part of the layout. */
  onClose?: () => void
  /** Shown in a drawer, which brings its own close button. */
  inDrawer?: boolean
}) {
  const tabs: SidebarTab[] = [
    ...projects.map((project): SidebarTab => ({ kind: 'project', project })),
    ...deployments.map((deployment): SidebarTab => ({
      kind: 'deployment',
      deployment,
    })),
  ].slice(0, MAX_TABS)
  const newest = deployments[0]
  const newestKey = newest ? key(newest) : ''
  const [selected, setSelected] = useState(
    newestKey || (conversationPublicId ? MADE_TAB : '')
  )
  // A deployment started later takes the focus; one the user picked stays put
  // until then.
  useEffect(() => {
    if (newestKey) setSelected(newestKey)
  }, [newestKey])
  const focusKey = focus?.key
  const focusNonce = focus?.nonce
  useEffect(() => {
    if (focusKey) setSelected(focusKey)
  }, [focusKey, focusNonce])

  if (tabs.length === 0 && !conversationPublicId) {
    return (
      <aside
        aria-label="Working tabs"
        className="flex h-full min-h-0 min-w-0 flex-col items-center justify-center gap-2 bg-background p-6 text-center"
      >
        <Rocket className="size-7 text-muted-foreground" aria-hidden="true" />
        <p className="text-sm font-medium">
          Nothing running from this chat yet
        </p>
        <p className="max-w-xs text-sm text-muted-foreground">
          When Temps deploys something for you, its build stages and link open
          here as a tab. A project named in a reply opens here too. Try
          &ldquo;Deploy my app from a Git repository URL&rdquo;.
        </p>
        {onClose && (
          <Button type="button" variant="outline" size="sm" onClick={onClose}>
            Close
          </Button>
        )}
      </aside>
    )
  }

  const firstKey = conversationPublicId ? MADE_TAB : tabKey(tabs[0])
  const selectable = [
    ...(conversationPublicId ? [MADE_TAB, PREVIEW_TAB] : []),
    ...tabs.map(tabKey),
  ]
  return (
    <aside
      aria-label="Working tabs"
      className="flex h-full min-h-0 min-w-0 flex-col bg-background"
    >
      <Tabs
        value={selectable.includes(selected) ? selected : firstKey}
        onValueChange={setSelected}
        className="flex min-h-0 flex-1 flex-col"
      >
        <div
          className={cn(
            'flex items-center gap-1 border-b px-2 py-2',
            inDrawer && 'pr-12'
          )}
        >
          <TabsList className="h-8 min-w-0 flex-1 justify-start overflow-x-auto">
            {conversationPublicId && (
              <TabsTrigger value={MADE_TAB} className="gap-1.5 text-xs">
                <Layers className="size-3.5 shrink-0" />
                Made here
              </TabsTrigger>
            )}
            {conversationPublicId && (
              <TabsTrigger value={PREVIEW_TAB} className="gap-1.5 text-xs">
                <Monitor className="size-3.5 shrink-0" />
                Preview
              </TabsTrigger>
            )}
            {tabs.map((tab) => (
              <TabsTrigger
                key={tabKey(tab)}
                value={tabKey(tab)}
                className="gap-1.5 text-xs"
              >
                {tab.kind === 'project' ? (
                  <>
                    <FolderGit2 className="size-3.5 shrink-0" />
                    <span className="max-w-32 truncate">
                      {tab.project.name}
                    </span>
                  </>
                ) : (
                  <>
                    <Rocket className="size-3.5 shrink-0" />
                    <TabLabel deployment={tab.deployment} />
                  </>
                )}
              </TabsTrigger>
            ))}
          </TabsList>
          {onClose && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="size-8 shrink-0"
              onClick={onClose}
              aria-label="Close working tabs"
            >
              <X className="size-4" />
            </Button>
          )}
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">
          {conversationPublicId && (
            <TabsContent value={MADE_TAB} className="mt-0">
              <ChatResourcesTab
                conversationPublicId={conversationPublicId}
                onOpenProject={onOpenProject ?? (() => undefined)}
              />
            </TabsContent>
          )}
          {conversationPublicId && (
            // Kept mounted so switching tabs does not reload the app.
            <TabsContent
              value={PREVIEW_TAB}
              forceMount
              className="mt-0 h-full data-[state=inactive]:hidden"
            >
              <ChatPreviewTab conversationPublicId={conversationPublicId} />
            </TabsContent>
          )}
          {tabs.map((tab) => (
            <TabsContent key={tabKey(tab)} value={tabKey(tab)} className="mt-0">
              {tab.kind === 'project' ? (
                <ProjectTab projectId={tab.project.id} />
              ) : (
                <DeploymentTab
                  projectId={tab.deployment.projectId}
                  deploymentId={tab.deployment.deploymentId}
                />
              )}
            </TabsContent>
          ))}
        </div>
      </Tabs>
    </aside>
  )
}

/** `project · #id`, or just the project while the goal follows its latest. */
function TabLabel({ deployment }: { deployment: WatchedDeployment }) {
  const project = useQuery(
    getProjectOptions({ path: { id: deployment.projectId } })
  )
  const name = project.data?.name ?? `Project ${deployment.projectId}`
  return (
    <span className="max-w-32 truncate">
      {deployment.deploymentId === null
        ? name
        : `${name} · #${deployment.deploymentId}`}
    </span>
  )
}

function key(d: WatchedDeployment): string {
  return `${d.projectId}:${d.deploymentId ?? 'latest'}`
}
