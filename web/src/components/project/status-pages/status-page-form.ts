// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import * as z from 'zod'
import type {
  MonitorResponse,
  StatusPageMonitorInput,
  StatusPageResponse,
} from '@/api/client'

/** Server-side limits, mirrored for inline validation. */
export const MAX_TITLE_CHARS = 120
export const MAX_DESCRIPTION_CHARS = 1000
export const MAX_DISPLAY_NAME_CHARS = 80
export const MAX_MONITORS_PER_PAGE = 50
export const MAX_INCIDENT_UPDATE_CHARS = 5000

export const INCIDENT_STATUSES = [
  'investigating',
  'identified',
  'monitoring',
  'resolved',
] as const

const monitorRowSchema = z
  .object({
    monitor_id: z.number(),
    monitor_name: z.string(),
    selected: z.boolean(),
    display_name: z.string(),
  })
  .superRefine((row, ctx) => {
    if (!row.selected) return
    const name = row.display_name.trim()
    if (name.length === 0) {
      ctx.addIssue({
        code: 'custom',
        path: ['display_name'],
        message: 'Give this monitor a public name',
      })
    } else if (name.length > MAX_DISPLAY_NAME_CHARS) {
      ctx.addIssue({
        code: 'custom',
        path: ['display_name'],
        message: `At most ${MAX_DISPLAY_NAME_CHARS} characters`,
      })
    }
  })

export const statusPageFormSchema = z.object({
  title: z
    .string()
    .trim()
    .min(1, 'Title is required')
    .max(MAX_TITLE_CHARS, `At most ${MAX_TITLE_CHARS} characters`),
  description: z
    .string()
    .max(MAX_DESCRIPTION_CHARS, `At most ${MAX_DESCRIPTION_CHARS} characters`),
  monitors: z
    .array(monitorRowSchema)
    .refine(
      (rows) => rows.filter((r) => r.selected).length <= MAX_MONITORS_PER_PAGE,
      `A status page can show at most ${MAX_MONITORS_PER_PAGE} monitors`
    ),
})

export type StatusPageFormValues = z.infer<typeof statusPageFormSchema>
export type MonitorRow = StatusPageFormValues['monitors'][number]

/**
 * One row per project monitor: selected monitors first, in their page order,
 * then the rest. Unselected rows start with an empty public name on purpose —
 * the internal monitor name is never pre-filled as the public one.
 */
export function buildMonitorRows(
  monitors: MonitorResponse[],
  page?: Pick<StatusPageResponse, 'monitors'>
): MonitorRow[] {
  const selected = [...(page?.monitors ?? [])].sort(
    (a, b) => a.position - b.position
  )
  const byId = new Map(monitors.map((m) => [m.id, m]))
  const rows: MonitorRow[] = selected.map((s) => ({
    monitor_id: s.monitor_id,
    monitor_name: byId.get(s.monitor_id)?.name ?? s.monitor_name,
    selected: true,
    display_name: s.display_name,
  }))
  const selectedIds = new Set(selected.map((s) => s.monitor_id))
  for (const monitor of monitors) {
    if (selectedIds.has(monitor.id)) continue
    rows.push({
      monitor_id: monitor.id,
      monitor_name: monitor.name,
      selected: false,
      display_name: '',
    })
  }
  return rows
}

/** The selection to send to the API, in display order. */
export function toSelection(rows: MonitorRow[]): StatusPageMonitorInput[] {
  return rows
    .filter((row) => row.selected)
    .map((row) => ({
      monitor_id: row.monitor_id,
      display_name: row.display_name.trim(),
    }))
}

export function formDefaults(
  monitors: MonitorResponse[],
  page?: StatusPageResponse
): StatusPageFormValues {
  return {
    title: page?.title ?? '',
    description: page?.description ?? '',
    monitors: buildMonitorRows(monitors, page),
  }
}

/**
 * Filter matching every cached query of one generated SDK operation. The
 * generated keys are `[{ _id, baseUrl, path, query, ... }]`, and TanStack
 * matches object keys partially, so `[{ _id }]` covers all variants.
 */
export function operationQueries(operationId: string) {
  return { queryKey: [{ _id: operationId }] }
}

export const incidentUpdateSchema = z.object({
  status: z.enum(INCIDENT_STATUSES),
  message: z
    .string()
    .trim()
    .min(1, 'Write a message for the update')
    .max(
      MAX_INCIDENT_UPDATE_CHARS,
      `At most ${MAX_INCIDENT_UPDATE_CHARS.toLocaleString('en-US')} characters`
    ),
  is_public: z.boolean(),
})

export type IncidentUpdateValues = z.infer<typeof incidentUpdateSchema>

/** The incident's current status if it is a known one, else "investigating". */
export function initialIncidentStatus(
  status: string
): IncidentUpdateValues['status'] {
  return (INCIDENT_STATUSES as readonly string[]).includes(status)
    ? (status as IncidentUpdateValues['status'])
    : 'investigating'
}
