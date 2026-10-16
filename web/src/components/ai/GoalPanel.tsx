// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { cancelGoal, retryGoal, type GoalResponse } from '@/api/client'
import {
  listGoalsOptions,
  listGoalsQueryKey,
} from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import { TimeAgo } from '@/components/utils/TimeAgo'
import { cn } from '@/lib/utils'
import { getErrorMessage } from '@/utils/errorHandling'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertTriangle,
  Ban,
  CheckCircle2,
  Loader2,
  RotateCcw,
  Target,
  XCircle,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  goalPollingInterval,
  presentGoal,
  visibleGoals,
  type GoalTone,
} from './goal-presentation'

const TONE_CLASSES: Record<GoalTone, string> = {
  working: 'text-sky-600 dark:text-sky-300',
  good: 'text-emerald-600 dark:text-emerald-300',
  attention: 'text-amber-600 dark:text-amber-300',
  bad: 'text-destructive',
  muted: 'text-muted-foreground',
}

function GoalIcon({
  status,
  className,
}: {
  status: string
  className: string
}) {
  switch (status) {
    case 'achieved':
      return <CheckCircle2 className={className} />
    case 'failed':
      return <XCircle className={className} />
    case 'needs_you':
      return <AlertTriangle className={className} />
    case 'cancelled':
      return <Ban className={className} />
    default:
      return <Loader2 className={cn(className, 'animate-spin')} />
  }
}

function GoalRow({
  goal,
  onStop,
  onRetry,
  busy,
}: {
  goal: GoalResponse
  onStop: () => void
  onRetry: () => void
  busy: boolean
}) {
  const view = presentGoal(goal)
  const tone = TONE_CLASSES[view.tone]
  return (
    <li className="flex items-start gap-2 px-3 py-2">
      <GoalIcon
        status={goal.status}
        className={cn('mt-0.5 size-4 shrink-0', tone)}
      />
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium">{goal.title}</p>
        <p className="text-xs text-muted-foreground">
          <span className={cn('font-medium', tone)}>{view.label}</span>
          {goal.last_observation ? ` · ${goal.last_observation}` : ''}
          {goal.last_checked_at ? (
            <>
              {' · checked '}
              <TimeAgo date={goal.last_checked_at} />
            </>
          ) : null}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-1">
        {view.canRetry && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 gap-1 px-2 text-xs"
            disabled={busy}
            onClick={onRetry}
          >
            <RotateCcw className="size-3" />
            Try again
          </Button>
        )}
        {view.canStop && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-xs text-muted-foreground"
            disabled={busy}
            onClick={onStop}
          >
            Stop
          </Button>
        )}
      </div>
    </li>
  )
}

/**
 * What this chat is keeping an eye on. The assistant registers a goal after it
 * starts something that can fail later (a deployment, a new service); Temps then
 * watches it and brings the assistant back to fix it. Renders nothing until the
 * chat has a goal: it is the result of a request, not a setting to find.
 */
export function GoalPanel({
  conversationPublicId,
}: {
  conversationPublicId: string
}) {
  const queryClient = useQueryClient()
  const options = { path: { public_id: conversationPublicId } }
  const goals = useQuery({
    ...listGoalsOptions(options),
    refetchInterval: (query) => goalPollingInterval(query.state.data ?? []),
  })
  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: listGoalsQueryKey(options) })

  const stop = useMutation({
    mutationFn: async (goalId: string) => {
      const { error } = await cancelGoal({
        path: { public_id: conversationPublicId, goal_id: goalId },
      })
      if (error) throw error
    },
    onSuccess: () => {
      toast.success('Stopped watching')
      void refresh()
    },
    onError: (error) =>
      toast.error(getErrorMessage(error, 'Could not stop the goal')),
  })
  const retry = useMutation({
    mutationFn: async (goalId: string) => {
      const { error } = await retryGoal({
        path: { public_id: conversationPublicId, goal_id: goalId },
      })
      if (error) throw error
    },
    onSuccess: () => {
      toast.success('Checking again')
      void refresh()
    },
    onError: (error) =>
      toast.error(getErrorMessage(error, 'Could not retry the goal')),
  })

  const shown = visibleGoals(goals.data ?? [])
  if (shown.length === 0) return null
  const busy = stop.isPending || retry.isPending
  return (
    <section
      aria-label="Goals Temps is watching"
      className="shrink-0 overflow-hidden rounded-xl border bg-card"
    >
      <h3 className="flex items-center gap-1.5 border-b px-3 py-1.5 text-xs font-medium text-muted-foreground">
        <Target className="size-3.5" />
        Watching for you
      </h3>
      <ul className="divide-y">
        {shown.map((goal) => (
          <GoalRow
            key={goal.id}
            goal={goal}
            busy={busy}
            onStop={() => stop.mutate(goal.id)}
            onRetry={() => retry.mutate(goal.id)}
          />
        ))}
      </ul>
    </section>
  )
}
