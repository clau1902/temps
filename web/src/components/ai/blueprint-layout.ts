// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Where each part of an architecture blueprint sits in its diagram.
 *
 * The layout is layered and computed, with no graph library: one column per
 * kind of part, in the order data travels (who uses it, the code Temps runs,
 * the state it keeps, the services outside), and apps ordered by role so the
 * pages sit above the API and the API above the workers. Flows are drawn as
 * curves between the facing edges of their two parts.
 */

export type BlueprintKind = 'client' | 'app' | 'data' | 'external'

export interface LayoutComponent {
  id: string
  kind: BlueprintKind
  role?: string | null
}

export interface LayoutFlow {
  from: string
  to: string
}

export interface PlacedNode {
  id: string
  x: number
  y: number
  width: number
  height: number
}

export interface PlacedEdge {
  index: number
  from: string
  to: string
  /** SVG path data for the curve. */
  path: string
  /** Where the flow's label sits. */
  labelX: number
  labelY: number
}

export interface BlueprintLayout {
  nodes: PlacedNode[]
  edges: PlacedEdge[]
  width: number
  height: number
  /** The column titles that have parts, with their x position. */
  columns: Array<{ kind: BlueprintKind; title: string; x: number }>
}

export const NODE_WIDTH = 168
export const NODE_HEIGHT = 58
const COLUMN_GAP = 92
const ROW_GAP = 22
const PADDING = 16
const HEADER = 22

const COLUMN_ORDER: BlueprintKind[] = ['client', 'app', 'data', 'external']

const COLUMN_TITLES: Record<BlueprintKind, string> = {
  client: 'Used from',
  app: 'Runs on Temps',
  data: 'Keeps state',
  external: 'Outside services',
}

const ROLE_ORDER = ['web', 'static', 'api', 'worker', 'cron']

function roleRank(role: string | null | undefined): number {
  const rank = ROLE_ORDER.indexOf(role ?? '')
  return rank === -1 ? ROLE_ORDER.length : rank
}

export function layoutBlueprint(
  components: readonly LayoutComponent[],
  flows: readonly LayoutFlow[]
): BlueprintLayout {
  const columns = COLUMN_ORDER.map((kind) =>
    components
      .map((component, index) => ({ component, index }))
      .filter(({ component }) => component.kind === kind)
      .sort(
        (a, b) =>
          (kind === 'app'
            ? roleRank(a.component.role) - roleRank(b.component.role)
            : 0) || a.index - b.index
      )
      .map(({ component }) => component)
  )
  const used = columns
    .map((parts, index) => ({ parts, kind: COLUMN_ORDER[index] }))
    .filter(({ parts }) => parts.length > 0)
  const tallest = Math.max(0, ...used.map(({ parts }) => parts.length))
  const contentHeight =
    tallest * NODE_HEIGHT + Math.max(0, tallest - 1) * ROW_GAP
  const nodes: PlacedNode[] = []
  const placed = new Map<string, PlacedNode>()
  used.forEach(({ parts }, column) => {
    const x = PADDING + column * (NODE_WIDTH + COLUMN_GAP)
    const height = parts.length * NODE_HEIGHT + (parts.length - 1) * ROW_GAP
    // Shorter columns are centred against the tallest one.
    const top = PADDING + HEADER + (contentHeight - height) / 2
    parts.forEach((part, row) => {
      const node = {
        id: part.id,
        x,
        y: top + row * (NODE_HEIGHT + ROW_GAP),
        width: NODE_WIDTH,
        height: NODE_HEIGHT,
      }
      nodes.push(node)
      placed.set(part.id, node)
    })
  })

  const edges: PlacedEdge[] = []
  flows.forEach((flow, index) => {
    const from = placed.get(flow.from)
    const to = placed.get(flow.to)
    if (!from || !to || from === to) return
    edges.push({ index, from: flow.from, to: flow.to, ...curve(from, to) })
  })

  const width =
    used.length === 0
      ? 0
      : PADDING * 2 + used.length * NODE_WIDTH + (used.length - 1) * COLUMN_GAP
  return {
    nodes,
    edges,
    width,
    height: used.length === 0 ? 0 : PADDING * 2 + HEADER + contentHeight,
    columns: used.map(({ kind }, column) => ({
      kind,
      title: COLUMN_TITLES[kind],
      x: PADDING + column * (NODE_WIDTH + COLUMN_GAP),
    })),
  }
}

function curve(
  from: PlacedNode,
  to: PlacedNode
): { path: string; labelX: number; labelY: number } {
  const fromCentre = { x: from.x + from.width / 2, y: from.y + from.height / 2 }
  const toCentre = { x: to.x + to.width / 2, y: to.y + to.height / 2 }
  if (from.x === to.x) {
    // Same column: leave from the right edge and come back in a loop.
    const startX = from.x + from.width
    const endX = to.x + to.width
    const bulge = startX + 36
    return {
      path: `M ${startX} ${fromCentre.y} C ${bulge} ${fromCentre.y}, ${bulge} ${toCentre.y}, ${endX} ${toCentre.y}`,
      labelX: bulge,
      labelY: (fromCentre.y + toCentre.y) / 2,
    }
  }
  const forward = to.x > from.x
  const startX = forward ? from.x + from.width : from.x
  const endX = forward ? to.x : to.x + to.width
  const midX = (startX + endX) / 2
  return {
    path: `M ${startX} ${fromCentre.y} C ${midX} ${fromCentre.y}, ${midX} ${toCentre.y}, ${endX} ${toCentre.y}`,
    labelX: midX,
    labelY: (fromCentre.y + toCentre.y) / 2,
  }
}

/**
 * Parts and flows a newer version adds, changes or removes compared with an
 * older one, for highlighting a proposal against what was accepted.
 */
export function blueprintChanges<
  C extends { id: string },
  F extends { from: string; to: string; kind: string; label: string },
>(
  previous: { components: readonly C[]; flows: readonly F[] } | null,
  next: { components: readonly C[]; flows: readonly F[] }
): {
  added: Set<string>
  changed: Set<string>
  removed: C[]
  addedFlows: Set<number>
  removedFlows: F[]
} {
  const flowKey = (flow: F) =>
    `${flow.from}>${flow.to}:${flow.kind}:${flow.label.trim()}`
  if (!previous) {
    return {
      added: new Set(),
      changed: new Set(),
      removed: [],
      addedFlows: new Set(),
      removedFlows: [],
    }
  }
  const before = new Map(previous.components.map((c) => [c.id, c]))
  const after = new Map(next.components.map((c) => [c.id, c]))
  const added = new Set<string>()
  const changed = new Set<string>()
  for (const component of next.components) {
    const old = before.get(component.id)
    if (!old) added.add(component.id)
    else if (JSON.stringify(old) !== JSON.stringify(component))
      changed.add(component.id)
  }
  const removed = previous.components.filter((c) => !after.has(c.id))
  const oldFlows = new Set(previous.flows.map(flowKey))
  const newFlows = new Set(next.flows.map(flowKey))
  const addedFlows = new Set<number>()
  next.flows.forEach((flow, index) => {
    if (!oldFlows.has(flowKey(flow))) addedFlows.add(index)
  })
  const removedFlows = previous.flows.filter((f) => !newFlows.has(flowKey(f)))
  return { added, changed, removed, addedFlows, removedFlows }
}

/**
 * The order to deploy the bound app parts in: an app another app calls is
 * deployed first, so a front end goes out after the API it talks to. Data and
 * outside parts are not deployed. Cycles fall back to the document order.
 */
export function deployOrder(
  components: readonly LayoutComponent[],
  flows: readonly LayoutFlow[]
): string[] {
  const apps = components.filter((c) => c.kind === 'app').map((c) => c.id)
  const appSet = new Set(apps)
  // `a` depends on `b` when a flow goes from `a` to the app `b`.
  const dependsOn = new Map(apps.map((id) => [id, new Set<string>()]))
  for (const flow of flows) {
    if (appSet.has(flow.from) && appSet.has(flow.to) && flow.from !== flow.to) {
      dependsOn.get(flow.from)?.add(flow.to)
    }
  }
  const order: string[] = []
  const done = new Set<string>()
  const visiting = new Set<string>()
  const visit = (id: string) => {
    if (done.has(id) || visiting.has(id)) return
    visiting.add(id)
    for (const dependency of dependsOn.get(id) ?? []) visit(dependency)
    visiting.delete(id)
    done.add(id)
    order.push(id)
  }
  apps.forEach(visit)
  return order
}
