// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  NODE_WIDTH,
  blueprintChanges,
  deployOrder,
  layoutBlueprint,
} from './blueprint-layout'

const parts = [
  { id: 'worker', kind: 'app' as const, role: 'worker' },
  { id: 'browser', kind: 'client' as const },
  { id: 'db', kind: 'data' as const },
  { id: 'api', kind: 'app' as const, role: 'api' },
  { id: 'web', kind: 'app' as const, role: 'web' },
  { id: 'mail', kind: 'external' as const },
]
const flows = [
  { from: 'browser', to: 'web', kind: 'http', label: 'pages' },
  { from: 'web', to: 'api', kind: 'http', label: '/api' },
  { from: 'api', to: 'db', kind: 'query', label: 'tasks' },
  { from: 'worker', to: 'mail', kind: 'http', label: 'send' },
  { from: 'web', to: 'nowhere', kind: 'http', label: 'dangling' },
]

describe('layoutBlueprint', () => {
  test('puts each kind in its own column, in the order data travels', () => {
    const layout = layoutBlueprint(parts, flows)
    const x = (id: string) => layout.nodes.find((n) => n.id === id)?.x ?? -1
    expect(x('browser')).toBeLessThan(x('web'))
    expect(x('web')).toBe(x('api'))
    expect(x('api')).toBeLessThan(x('db'))
    expect(x('db')).toBeLessThan(x('mail'))
    expect(layout.columns.map((c) => c.kind)).toEqual([
      'client',
      'app',
      'data',
      'external',
    ])
  })

  test('orders apps by role: pages, then the API, then workers', () => {
    const layout = layoutBlueprint(parts, flows)
    const y = (id: string) => layout.nodes.find((n) => n.id === id)?.y ?? -1
    expect(y('web')).toBeLessThan(y('api'))
    expect(y('api')).toBeLessThan(y('worker'))
  })

  test('draws only flows between parts that exist', () => {
    const layout = layoutBlueprint(parts, flows)
    expect(layout.edges.map((e) => e.index)).toEqual([0, 1, 2, 3])
    expect(layout.edges[0].path.startsWith('M ')).toBe(true)
  })

  test('skips empty columns so the diagram stays narrow', () => {
    const layout = layoutBlueprint(
      [
        { id: 'web', kind: 'app', role: 'static' },
        { id: 'browser', kind: 'client' },
      ],
      []
    )
    expect(layout.columns).toHaveLength(2)
    expect(layout.width).toBeLessThan(3 * NODE_WIDTH + 200)
  })

  test('an empty design has no size', () => {
    expect(layoutBlueprint([], [])).toMatchObject({ width: 0, height: 0 })
  })
})

describe('blueprintChanges', () => {
  test('names what a proposal adds, changes and removes', () => {
    const previous = {
      components: [
        { id: 'web', name: 'Web' },
        { id: 'db', name: 'DB' },
        { id: 'old', name: 'Old' },
      ],
      flows: [{ from: 'web', to: 'db', kind: 'query', label: 'tasks' }],
    }
    const next = {
      components: [
        { id: 'web', name: 'Web app' },
        { id: 'db', name: 'DB' },
        { id: 'cache', name: 'Cache' },
      ],
      flows: [
        { from: 'web', to: 'db', kind: 'query', label: 'tasks' },
        { from: 'web', to: 'cache', kind: 'cache', label: 'sessions' },
      ],
    }
    const changes = blueprintChanges(previous, next)
    expect([...changes.added]).toEqual(['cache'])
    expect([...changes.changed]).toEqual(['web'])
    expect(changes.removed.map((c) => c.id)).toEqual(['old'])
    expect([...changes.addedFlows]).toEqual([1])
    expect(changes.removedFlows).toEqual([])
  })

  test('a first design has nothing to compare against', () => {
    const changes = blueprintChanges(null, { components: [], flows: [] })
    expect(changes.added.size).toBe(0)
  })
})

describe('deployOrder', () => {
  test('deploys an API before the front end that calls it', () => {
    expect(deployOrder(parts, flows)).toEqual(['worker', 'api', 'web'])
  })

  test('a cycle still lists every app once', () => {
    const order = deployOrder(
      [
        { id: 'a', kind: 'app' },
        { id: 'b', kind: 'app' },
      ],
      [
        { from: 'a', to: 'b' },
        { from: 'b', to: 'a' },
      ]
    )
    expect(order.sort()).toEqual(['a', 'b'])
  })
})
