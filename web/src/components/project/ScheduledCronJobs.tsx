// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useContext, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { CalendarClock, FileCode } from 'lucide-react'
import {
  Button,
  Callout,
  DataTable,
  PageState,
  RecordLink,
} from '@temps-sdk/ds'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@temps-sdk/ui'
import type { CronInfo, ProjectResponse } from '@/api/client'
import {
  getEnvironmentCronsOptions,
  getEnvironmentsOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { CodeBlock } from '@/components/ui/code-block'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { ProjectEnvironmentContext } from '@/components/project-v2/project-environment-context'
import { readFailureServerDetail } from '@/lib/read-failure'

function InlineCode({ children }: { children: string }) {
  return (
    <code className="rounded bg-muted px-1 py-0.5 text-xs">{children}</code>
  )
}

/** How to declare a job in `.temps.yaml`; shown in the "How to add" dialog. */
function Instructions() {
  return (
    <div className="space-y-4">
      <p className="text-sm">
        List the jobs under <InlineCode>cron:</InlineCode> in the{' '}
        <InlineCode>.temps.yaml</InlineCode> file at your repository root:
      </p>
      <CodeBlock
        language="yaml"
        disableWrapToggle
        code={`cron:
  - path: "/api/ping"
    schedule: "*/5 * * * *"    # Every 5 minutes

  - path: "/api/daily-backup"
    schedule: "0 0 * * *"      # Daily at midnight

  - path: "/api/weekly-report"
    schedule: "0 0 * * 0"      # Weekly on Sunday`}
      />
      <ul className="list-inside list-disc space-y-1 text-sm text-muted-foreground">
        <li>
          <InlineCode>schedule</InlineCode> uses the five cron fields: minute,
          hour, day of month, month, weekday.
        </li>
        <li>
          <InlineCode>path</InlineCode> is an endpoint of your app; Temps calls
          it with a POST request.
        </li>
        <li>Changes apply on your next deployment.</li>
      </ul>
    </div>
  )
}

function formatRun(value: string | null | undefined) {
  return value ? new Date(value).toLocaleString() : 'Not scheduled'
}

/**
 * Jobs Temps runs itself: the `cron:` entries of `.temps.yaml`, registered
 * per environment on deploy. In the v2 layout the environment comes from the
 * project bar; the classic layout keeps a picker of its own.
 */
export function ScheduledCronJobs({ project }: { project: ProjectResponse }) {
  const bar = useContext(ProjectEnvironmentContext)
  const [pickedEnvironment, setPickedEnvironment] = useState('')
  const [showInstructions, setShowInstructions] = useState(false)

  const environments = useQuery({
    ...getEnvironmentsOptions({ path: { project_id: project.id } }),
    enabled: !bar,
  })
  const environment = bar
    ? bar.selection.environment
    : (environments.data?.find(
        (env) => env.id.toString() === pickedEnvironment
      ) ?? environments.data?.[0])

  const crons = useQuery({
    ...getEnvironmentCronsOptions({
      path: { project_id: project.id, env_id: environment?.id ?? 0 },
    }),
    enabled: environment !== undefined,
  })
  const loading =
    (bar ? bar.isPending : environments.isPending) ||
    (environment !== undefined && crons.isPending)
  const rows = crons.data ?? []

  return (
    <section aria-labelledby="scheduled-cron-jobs" className="space-y-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <h2 id="scheduled-cron-jobs" className="text-base font-semibold">
            Scheduled by Temps
          </h2>
          <p className="text-sm text-muted-foreground">
            Endpoints of your app that Temps calls on a schedule, from{' '}
            <InlineCode>cron:</InlineCode> in{' '}
            <InlineCode>.temps.yaml</InlineCode>
            {environment ? ` (${environment.name})` : ''}.
          </p>
        </div>
        <div className="flex flex-col gap-2 sm:flex-row">
          {!bar && (
            <Select
              value={environment?.id.toString() ?? ''}
              onValueChange={setPickedEnvironment}
              disabled={!environments.data?.length}
            >
              <SelectTrigger
                className="w-full sm:w-[200px]"
                aria-label="Environment"
              >
                <SelectValue placeholder="Environment" />
              </SelectTrigger>
              <SelectContent>
                {environments.data?.map((env) => (
                  <SelectItem key={env.id} value={env.id.toString()}>
                    {env.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
          <Button variant="outline" onClick={() => setShowInstructions(true)}>
            <FileCode className="size-4" />
            How to add a job
          </Button>
        </div>
      </div>

      {crons.isError ? (
        <Callout tone="error" title="Could not load scheduled jobs">
          {readFailureServerDetail(crons.error) ??
            `The scheduled jobs of ${environment?.name ?? 'this environment'} did not load.`}{' '}
          <button
            type="button"
            className="underline underline-offset-4"
            onClick={() => void crons.refetch()}
          >
            Retry
          </button>
        </Callout>
      ) : !loading && rows.length === 0 ? (
        <PageState
          variant="empty"
          size="compact"
          icon={CalendarClock}
          title="No scheduled jobs"
          description={
            environment
              ? `${environment.name} has no cron: entries in .temps.yaml as of its last deployment.`
              : 'This service has no environments yet.'
          }
          action={
            <Button variant="outline" onClick={() => setShowInstructions(true)}>
              How to add a job
            </Button>
          }
        />
      ) : (
        <DataTable<CronInfo>
          rows={rows}
          rowKey={(cron) => cron.id}
          isLoading={loading}
          columns={[
            {
              key: 'path',
              header: 'Endpoint',
              render: (cron) => (
                <RecordLink
                  className="font-mono"
                  to={`/projects/${project.slug}/settings/cron-jobs/${cron.environment_id}/${cron.id}`}
                >
                  {cron.path}
                </RecordLink>
              ),
            },
            {
              key: 'schedule',
              header: 'Schedule',
              render: (cron) => (
                <code className="text-sm">{cron.schedule}</code>
              ),
            },
            {
              key: 'next',
              header: 'Next run',
              render: (cron) => formatRun(cron.next_run),
            },
          ]}
        />
      )}

      <Dialog open={showInstructions} onOpenChange={setShowInstructions}>
        <DialogContent className="sm:max-w-[500px]">
          <DialogHeader>
            <DialogTitle>Schedule a job</DialogTitle>
            <DialogDescription>
              Temps calls your endpoint on the schedule you set.
            </DialogDescription>
          </DialogHeader>
          <Instructions />
        </DialogContent>
      </Dialog>
    </section>
  )
}
