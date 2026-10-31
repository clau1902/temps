// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { FileCode } from 'lucide-react'
import { Button, Callout, DataTable, RecordLink } from '@temps-sdk/ds'
import type { CronInfo, ProjectResponse } from '@/api/client'
import { getEnvironmentCronsOptions } from '@/api/client/@tanstack/react-query.gen'
import { CodeBlock } from '@/components/ui/code-block'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { useProjectEnvironment } from '@/components/project-frame/project-environment-context'
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
 * The `cron:` entries of `.temps.yaml` for the environment chosen in the
 * project bar. Shared by the section and the cron page, which needs to know
 * whether there is anything at all before it decides how to lay itself out.
 */
export function useScheduledCronJobs(project: ProjectResponse) {
  const bar = useProjectEnvironment()
  const environment = bar.selection.environment
  const crons = useQuery({
    ...getEnvironmentCronsOptions({
      path: { project_id: project.id, env_id: environment?.id ?? 0 },
    }),
    enabled: environment !== undefined,
  })
  return {
    bar,
    environment,
    crons,
    rows: crons.data ?? [],
    loading: bar.isPending || (environment !== undefined && crons.isPending),
    failed: bar.isError || crons.isError,
  }
}

/** How to declare a job in `.temps.yaml`. */
export function ScheduleJobDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
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
  )
}

/**
 * Jobs Temps runs itself: the `cron:` entries of `.temps.yaml`, registered
 * per environment on deploy, for the environment chosen in the project bar.
 */
export function ScheduledCronJobs({ project }: { project: ProjectResponse }) {
  const { bar, environment, crons, rows, loading } =
    useScheduledCronJobs(project)
  const [showInstructions, setShowInstructions] = useState(false)

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
          <Button variant="outline" onClick={() => setShowInstructions(true)}>
            <FileCode className="size-4" />
            How to add a job
          </Button>
        </div>
      </div>

      {bar.isError ? (
        <Callout tone="error" title="Could not load environments">
          The scheduled jobs are listed per environment, and this service&apos;s
          environments did not load.{' '}
          <button
            type="button"
            className="underline underline-offset-4"
            onClick={bar.retry}
          >
            Retry
          </button>
        </Callout>
      ) : crons.isError ? (
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
        // The header already offers "How to add a job"; one quiet line is
        // enough while the page has check-in monitors to show.
        <p className="text-sm text-muted-foreground">
          {environment
            ? `No scheduled jobs in ${environment.name} as of its last deployment.`
            : 'This service has no environments yet.'}
        </p>
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

      <ScheduleJobDialog
        open={showInstructions}
        onOpenChange={setShowInstructions}
      />
    </section>
  )
}
