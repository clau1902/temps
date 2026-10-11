// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  getIncidentUpdatesOptions,
  listIncidentsOptions,
  setIncidentUpdateVisibilityMutation,
  updateIncidentStatusMutation,
} from '@/api/client/@tanstack/react-query.gen'
import type { IncidentResponse, ProjectResponse } from '@/api/client'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { ErrorAlert } from '@/components/utils/ErrorAlert'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { formatDistanceToNow } from 'date-fns'
import { useId, useState } from 'react'
import { toast } from 'sonner'
import { operationQueries } from './status-page-form'

const INCIDENT_STATUSES = [
  'investigating',
  'identified',
  'monitoring',
  'resolved',
] as const

function titleCase(value: string) {
  return value.charAt(0).toUpperCase() + value.slice(1)
}

function PostUpdateForm({ incident }: { incident: IncidentResponse }) {
  const queryClient = useQueryClient()
  const id = useId()
  const [status, setStatus] = useState<string>(incident.status)
  const [message, setMessage] = useState('')
  const [isPublic, setIsPublic] = useState(true)
  const post = useMutation({
    ...updateIncidentStatusMutation(),
    meta: { errorTitle: 'Failed to post incident update' },
    onSuccess: async () => {
      await queryClient.invalidateQueries(
        operationQueries('getIncidentUpdates')
      )
      await queryClient.invalidateQueries(operationQueries('listIncidents'))
      setMessage('')
      toast.success(
        isPublic
          ? 'Update posted and shown on published status pages.'
          : 'Private update posted.'
      )
    },
  })
  return (
    <form
      className="space-y-2"
      onSubmit={(event) => {
        event.preventDefault()
        if (!message.trim()) {
          toast.error('Write a message for the update.')
          return
        }
        post.mutate({
          path: { incident_id: incident.id },
          body: { status, message, is_public: isPublic },
        })
      }}
    >
      <Textarea
        rows={2}
        value={message}
        onChange={(e) => setMessage(e.target.value)}
        placeholder="We have identified the cause and are rolling out a fix."
        aria-label="Update message"
      />
      <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
        <Select value={status} onValueChange={setStatus}>
          <SelectTrigger className="w-full sm:w-[180px]" aria-label="Status">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {INCIDENT_STATUSES.map((s) => (
              <SelectItem key={s} value={s}>
                {titleCase(s)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="flex items-center gap-2">
          <Checkbox
            id={`${id}-public`}
            checked={isPublic}
            onCheckedChange={(checked) => setIsPublic(checked === true)}
          />
          <Label htmlFor={`${id}-public`}>Show on status pages</Label>
        </div>
        <Button
          type="submit"
          size="sm"
          className="sm:ml-auto"
          disabled={post.isPending}
        >
          {post.isPending ? 'Posting...' : 'Post update'}
        </Button>
      </div>
    </form>
  )
}

function IncidentRow({ incident }: { incident: IncidentResponse }) {
  const queryClient = useQueryClient()
  const updates = useQuery(
    getIncidentUpdatesOptions({ path: { incident_id: incident.id } })
  )
  const toggle = useMutation({
    ...setIncidentUpdateVisibilityMutation(),
    meta: { errorTitle: 'Failed to change update visibility' },
    onSuccess: async (update) => {
      await queryClient.invalidateQueries(
        operationQueries('getIncidentUpdates')
      )
      toast.success(
        update.is_public
          ? 'Update is now shown on published status pages.'
          : 'Update is now private.'
      )
    },
  })

  return (
    <div className="space-y-3 p-4">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{incident.title}</span>
        <Badge variant="neutral-subtle">{titleCase(incident.severity)}</Badge>
        <Badge
          variant={
            incident.status === 'resolved' ? 'success-subtle' : 'warning-subtle'
          }
        >
          {titleCase(incident.status)}
        </Badge>
        <span className="text-xs text-muted-foreground">
          started{' '}
          {formatDistanceToNow(new Date(incident.started_at), {
            addSuffix: true,
          })}
        </span>
      </div>
      <p className="text-xs text-muted-foreground">
        The incident title and description stay private. A status page shows
        only the updates switched to public, under a heading derived from the
        severity.
      </p>
      {updates.isLoading ? (
        <Skeleton className="h-10 w-full" />
      ) : updates.isError ? (
        <p className="text-sm text-destructive">Failed to load updates.</p>
      ) : (
        <ul className="space-y-2">
          {(updates.data ?? []).map((update) => (
            <li
              key={update.id}
              className="flex flex-col gap-2 border-l-2 pl-3 sm:flex-row sm:items-start sm:justify-between"
            >
              <div className="min-w-0">
                <div className="text-xs text-muted-foreground">
                  {titleCase(update.status)} ·{' '}
                  {formatDistanceToNow(new Date(update.created_at), {
                    addSuffix: true,
                  })}
                </div>
                <p className="whitespace-pre-wrap break-words text-sm">
                  {update.message}
                </p>
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <Switch
                  checked={update.is_public}
                  disabled={toggle.isPending || update.is_system}
                  aria-label="Show this update on status pages"
                  onCheckedChange={(checked) =>
                    toggle.mutate({
                      path: {
                        incident_id: incident.id,
                        update_id: update.id,
                      },
                      body: { is_public: checked },
                    })
                  }
                />
                <span className="text-xs text-muted-foreground">
                  {update.is_system
                    ? 'Private (automatic)'
                    : update.is_public
                      ? 'Public'
                      : 'Private'}
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
      <PostUpdateForm incident={incident} />
    </div>
  )
}

/**
 * Recent incidents of the project, with the controls that decide what of them
 * reaches published status pages. Incident updates are private by default.
 */
export function StatusPageIncidents({ project }: { project: ProjectResponse }) {
  const incidents = useQuery(
    listIncidentsOptions({
      path: { project_id: project.id },
      query: { page: 1, page_size: 5 },
    })
  )
  const items = incidents.data?.incidents ?? []

  return (
    <section className="space-y-3">
      <div>
        <h3 className="text-lg font-semibold">Incident updates</h3>
        <p className="text-sm text-muted-foreground">
          Incident updates are private unless you switch them to public. Public
          updates appear on this project&apos;s published status pages when the
          incident concerns a monitor shown there, or the whole project.
        </p>
      </div>
      {incidents.isError ? (
        <ErrorAlert
          title="Failed to load incidents"
          description={
            incidents.error instanceof Error
              ? incidents.error.message
              : 'An unexpected error occurred'
          }
          retry={() => incidents.refetch()}
        />
      ) : incidents.isLoading ? (
        <Skeleton className="h-24 w-full" />
      ) : items.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No incidents. When a monitor detects an outage, or you report one
          through the API or CLI (<code>temps incidents create</code>), it
          appears here so you can post public updates.
        </p>
      ) : (
        <Card>
          <CardContent className="divide-y p-0">
            {items.map((incident) => (
              <IncidentRow key={incident.id} incident={incident} />
            ))}
          </CardContent>
        </Card>
      )}
    </section>
  )
}
