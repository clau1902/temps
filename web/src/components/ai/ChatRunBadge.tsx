// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { cn } from '@/lib/utils'
import {
  CircleAlert,
  CircleCheck,
  CircleStop,
  Loader2,
  type LucideIcon,
} from 'lucide-react'
import {
  chatRunPresentation,
  chatRunState,
  type ChatRunState,
} from './chat-run-state'

const RUN_STATE_STYLES: Record<
  ChatRunState,
  { Icon: LucideIcon; className: string; iconClassName?: string }
> = {
  working: {
    Icon: Loader2,
    className: 'text-blue-600 dark:text-blue-400',
    iconClassName: 'animate-spin',
  },
  finished: {
    Icon: CircleCheck,
    className: 'text-emerald-600 dark:text-emerald-400',
  },
  error: { Icon: CircleAlert, className: 'text-destructive' },
  stopped: { Icon: CircleStop, className: 'text-muted-foreground' },
  interrupted: {
    Icon: CircleAlert,
    className: 'text-amber-600 dark:text-amber-400',
  },
}

/** Whether the chat's latest turn is working, finished, failed or stopped. */
export function ChatRunBadge({
  turnStatus,
  failureTitle,
  compact = false,
}: {
  turnStatus: string
  failureTitle?: string | null
  /** Icon only, with the label for screen readers and on hover. */
  compact?: boolean
}) {
  const state = chatRunState(turnStatus)
  if (!state) return null
  const { label, title } = chatRunPresentation(state, failureTitle)
  const { Icon, className, iconClassName } = RUN_STATE_STYLES[state]
  return (
    <span
      className={cn('flex shrink-0 items-center gap-1 font-medium', className)}
      title={title}
    >
      <Icon className={cn('size-3', iconClassName)} aria-hidden />
      <span className={compact ? 'sr-only' : undefined}>{label}</span>
    </span>
  )
}
