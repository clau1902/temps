// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  goalPollingInterval,
  presentGoal,
  visibleGoals,
} from './goal-presentation'

const goal = (status: string, attempts = 0) => ({
  status,
  attempts,
  max_attempts: 3,
})

describe('presentGoal', () => {
  test('says how far a fix has got', () => {
    expect(presentGoal(goal('healing', 2)).label).toBe(
      'Fixing · attempt 2 of 3'
    )
  })

  test('only open goals can be stopped, only stuck ones retried', () => {
    expect(presentGoal(goal('active'))).toMatchObject({
      canStop: true,
      canRetry: false,
    })
    expect(presentGoal(goal('needs_you'))).toMatchObject({
      canStop: true,
      canRetry: true,
    })
    expect(presentGoal(goal('failed'))).toMatchObject({
      canStop: false,
      canRetry: true,
    })
    for (const status of ['achieved', 'cancelled']) {
      expect(presentGoal(goal(status))).toMatchObject({
        canStop: false,
        canRetry: false,
        open: false,
      })
    }
  })

  test('shows an unknown status as it is instead of hiding it', () => {
    expect(presentGoal(goal('paused')).label).toBe('paused')
  })
})

describe('goalPollingInterval', () => {
  test('polls faster while something is watched', () => {
    expect(goalPollingInterval([goal('active')])).toBe(4000)
    expect(goalPollingInterval([goal('achieved')])).toBe(15000)
    expect(goalPollingInterval([])).toBe(15000)
  })
})

describe('visibleGoals', () => {
  test('keeps every open goal and only the latest closed ones', () => {
    const goals = [
      goal('achieved'),
      goal('active'),
      goal('failed'),
      goal('cancelled'),
      goal('needs_you'),
    ]
    expect(visibleGoals(goals).map((g) => g.status)).toEqual([
      'active',
      'needs_you',
      'achieved',
      'failed',
    ])
  })
})
