// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { GoalResponse } from '@/api/client'

export type GoalTone = 'working' | 'good' | 'attention' | 'bad' | 'muted'

export interface GoalPresentation {
  label: string
  tone: GoalTone
  /** Still being observed by the server. */
  open: boolean
  canStop: boolean
  canRetry: boolean
}

/** What a goal's status means to the person reading the chat. */
export function presentGoal(
  goal: Pick<GoalResponse, 'status' | 'attempts' | 'max_attempts'>
): GoalPresentation {
  switch (goal.status) {
    case 'active':
      return {
        label: 'Watching',
        tone: 'working',
        open: true,
        canStop: true,
        canRetry: false,
      }
    case 'healing':
      return {
        label: `Fixing · attempt ${goal.attempts} of ${goal.max_attempts}`,
        tone: 'working',
        open: true,
        canStop: true,
        canRetry: false,
      }
    case 'needs_you':
      return {
        label: 'Needs you',
        tone: 'attention',
        open: true,
        canStop: true,
        canRetry: true,
      }
    case 'achieved':
      return {
        label: 'Done',
        tone: 'good',
        open: false,
        canStop: false,
        canRetry: false,
      }
    case 'failed':
      return {
        label: 'Gave up',
        tone: 'bad',
        open: false,
        canStop: false,
        canRetry: true,
      }
    case 'cancelled':
      return {
        label: 'Stopped',
        tone: 'muted',
        open: false,
        canStop: false,
        canRetry: false,
      }
    default:
      return {
        label: goal.status,
        tone: 'muted',
        open: false,
        canStop: false,
        canRetry: false,
      }
  }
}

/** Poll quickly while something is being watched, slowly otherwise. */
export function goalPollingInterval(
  goals: ReadonlyArray<
    Pick<GoalResponse, 'status' | 'attempts' | 'max_attempts'>
  >
): number {
  return goals.some((goal) => presentGoal(goal).open) ? 4000 : 15000
}

/** Goals worth a row: everything open, plus the few most recent that closed. */
export function visibleGoals<
  T extends Pick<GoalResponse, 'status' | 'attempts' | 'max_attempts'>,
>(goals: readonly T[], closedToShow = 2): T[] {
  const open = goals.filter((goal) => presentGoal(goal).open)
  const closed = goals
    .filter((goal) => !presentGoal(goal).open)
    .slice(0, closedToShow)
  return [...open, ...closed]
}
