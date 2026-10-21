// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type {
  AppRole,
  AppRuntime,
  BlueprintDocument,
  ClientSurface,
  Component,
  ComponentKind,
  DataEngine,
  Flow,
  FlowKind,
} from '@/api/client/types.gen'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Textarea } from '@/components/ui/textarea'
import { Loader2, Plus, Trash2 } from 'lucide-react'
import { useId, useState } from 'react'
import {
  cleanDocument,
  formatHolds,
  newComponent,
  parseHolds,
  removeComponent,
  updateComponent,
  withKind,
} from './blueprint-editor'

const KINDS: Array<{ value: ComponentKind; label: string }> = [
  { value: 'app', label: 'App' },
  { value: 'data', label: 'Data' },
  { value: 'client', label: 'Client' },
  { value: 'external', label: 'Outside service' },
]
const ROLES: AppRole[] = ['web', 'api', 'worker', 'cron', 'static']
const RUNTIMES: AppRuntime[] = ['preset', 'dockerfile', 'image']
const ENGINES: DataEngine[] = ['postgres', 'redis', 'mongodb', 'mariadb', 's3']
const SURFACES: ClientSurface[] = ['browser', 'mobile', 'cli', 'third_party']
const FLOW_KINDS: FlowKind[] = [
  'http',
  'websocket',
  'query',
  'publish',
  'subscribe',
  'cache',
  'blob',
  'webhook',
]

function Choice<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string
  value: T | null | undefined
  options: ReadonlyArray<T | { value: T; label: string }>
  onChange: (value: T) => void
}) {
  const id = useId()
  return (
    <div className="space-y-1">
      <Label htmlFor={id} className="text-xs">
        {label}
      </Label>
      <Select
        value={value ?? undefined}
        onValueChange={(v) => onChange(v as T)}
      >
        <SelectTrigger id={id} className="h-8 text-xs">
          <SelectValue placeholder="Choose" />
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => {
            const item =
              typeof option === 'string'
                ? { value: option, label: option.replace('_', ' ') }
                : option
            return (
              <SelectItem
                key={item.value}
                value={item.value}
                className="text-xs"
              >
                {item.label}
              </SelectItem>
            )
          })}
        </SelectContent>
      </Select>
    </div>
  )
}

function Field({
  label,
  value,
  placeholder,
  onChange,
}: {
  label: string
  value: string
  placeholder?: string
  onChange: (value: string) => void
}) {
  const id = useId()
  return (
    <div className="space-y-1">
      <Label htmlFor={id} className="text-xs">
        {label}
      </Label>
      <Input
        id={id}
        value={value}
        placeholder={placeholder}
        onChange={(event) => onChange(event.target.value)}
        className="h-8 text-xs"
      />
    </div>
  )
}

/**
 * The comma-separated list of what a data part holds. The field shows exactly
 * what was typed (spaces, a trailing comma) and the part gets the parsed list;
 * rebuilding the text from the list on every keystroke would add a space after
 * each comma and throw the caret to the end. It starts from the part's list
 * whenever it mounts, which is when the part (keyed by id) or its kind changes.
 */
function HoldsField({
  holds,
  onChange,
}: {
  holds: string[] | null | undefined
  onChange: (holds: string[]) => void
}) {
  const [text, setText] = useState(() => formatHolds(holds))
  return (
    <Field
      label="Holds (comma separated)"
      value={text}
      placeholder="tasks, users"
      onChange={(next) => {
        setText(next)
        onChange(parseHolds(next))
      }}
    />
  )
}

function PartEditor({
  component,
  onChange,
  onRemove,
}: {
  component: Component
  onChange: (next: Component) => void
  onRemove: () => void
}) {
  const set = (patch: Partial<Component>) =>
    onChange({ ...component, ...patch })
  return (
    <fieldset className="space-y-2 rounded-md border p-3">
      <legend className="flex items-center gap-2 px-1 text-xs font-medium">
        <code>{component.id}</code>
      </legend>
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
        <Field
          label="Name"
          value={component.name}
          onChange={(name) => set({ name })}
        />
        <Choice
          label="Kind"
          value={component.kind}
          options={KINDS}
          onChange={(kind) => onChange(withKind(component, kind))}
        />
      </div>
      <Field
        label="Why it exists"
        value={component.purpose}
        placeholder="Serves the pages and the sign-in"
        onChange={(purpose) => set({ purpose })}
      />
      {component.kind === 'client' && (
        <Choice
          label="Used from"
          value={component.surface}
          options={SURFACES}
          onChange={(surface) => set({ surface })}
        />
      )}
      {component.kind === 'app' && (
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
          <Choice
            label="Role"
            value={component.role}
            options={ROLES}
            onChange={(role) => set({ role })}
          />
          <Choice
            label="Runs as"
            value={component.runtime}
            options={RUNTIMES}
            onChange={(runtime) => set({ runtime })}
          />
          <Field
            label="Stack"
            value={component.stack ?? ''}
            placeholder="Bun + Hono"
            onChange={(stack) => set({ stack })}
          />
          {component.runtime === 'image' && (
            <Field
              label="Image"
              value={component.image ?? ''}
              placeholder="nats:2.10"
              onChange={(image) => set({ image })}
            />
          )}
        </div>
      )}
      {component.kind === 'data' && (
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
          <Choice
            label="Engine"
            value={component.engine}
            options={ENGINES}
            onChange={(engine) => set({ engine })}
          />
          <HoldsField
            holds={component.holds}
            onChange={(holds) => set({ holds })}
          />
        </div>
      )}
      {component.kind === 'external' && (
        <Field
          label="Provider"
          value={component.provider ?? ''}
          placeholder="Stripe"
          onChange={(provider) => set({ provider })}
        />
      )}
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="h-7 gap-1.5 px-2 text-xs text-destructive"
        onClick={onRemove}
      >
        <Trash2 className="size-3.5" />
        Remove {component.name || component.id}
      </Button>
    </fieldset>
  )
}

function FlowEditor({
  flow,
  parts,
  onChange,
  onRemove,
}: {
  flow: Flow
  parts: Component[]
  onChange: (next: Flow) => void
  onRemove: () => void
}) {
  const options = parts.map((p) => ({ value: p.id, label: p.name || p.id }))
  return (
    <div className="grid grid-cols-2 items-end gap-2 rounded-md border p-2 sm:grid-cols-[1fr_1fr_7rem_1.5fr_auto]">
      <Choice
        label="From"
        value={flow.from}
        options={options}
        onChange={(from) => onChange({ ...flow, from })}
      />
      <Choice
        label="To"
        value={flow.to}
        options={options}
        onChange={(to) => onChange({ ...flow, to })}
      />
      <Choice
        label="Carries"
        value={flow.kind}
        options={FLOW_KINDS}
        onChange={(kind) => onChange({ ...flow, kind })}
      />
      <Field
        label="What travels"
        value={flow.label}
        placeholder="POST /api/tasks"
        onChange={(label) => onChange({ ...flow, label })}
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="size-8 text-destructive"
        aria-label={`Remove the flow from ${flow.from} to ${flow.to}`}
        onClick={onRemove}
      >
        <Trash2 className="size-3.5" />
      </Button>
    </div>
  )
}

/**
 * The user's own edit of the design. Saving records a new version that is
 * accepted at once, since the user wrote it; the server checks it the same way
 * it checks a proposal and lists every problem it finds.
 */
export function BlueprintEditor({
  initial,
  initialChangeSummary = '',
  saving,
  problems,
  onSave,
  onCancel,
}: {
  initial: BlueprintDocument
  /** What the user said they changed, when resuming a refused save. */
  initialChangeSummary?: string
  saving: boolean
  problems: string[]
  onSave: (document: BlueprintDocument, changeSummary: string) => void
  onCancel: () => void
}) {
  const [draft, setDraft] = useState<BlueprintDocument>(() => ({
    ...initial,
    flows: initial.flows ?? [],
  }))
  const [summary, setSummary] = useState(initialChangeSummary)
  const [newKind, setNewKind] = useState<ComponentKind>('app')
  const [newName, setNewName] = useState('')
  const flows = draft.flows ?? []
  const ids = draft.components.map((c) => c.id)
  const addPart = () => {
    const name = newName.trim()
    if (!name) return
    setDraft({
      ...draft,
      components: [...draft.components, newComponent(newKind, name, ids)],
    })
    setNewName('')
  }
  const addFlow = () => {
    const [first, second] = draft.components
    if (!first || !second) return
    setDraft({
      ...draft,
      flows: [
        ...flows,
        { from: first.id, to: second.id, kind: 'http', label: '' },
      ],
    })
  }
  const setFlow = (index: number, next: Flow) =>
    setDraft({ ...draft, flows: flows.map((f, i) => (i === index ? next : f)) })
  const removeFlow = (index: number) =>
    setDraft({ ...draft, flows: flows.filter((_, i) => i !== index) })

  return (
    <form
      className="space-y-4"
      onSubmit={(event) => {
        event.preventDefault()
        onSave(cleanDocument(draft), summary.trim())
      }}
    >
      <div className="space-y-1">
        <Label htmlFor="blueprint-summary" className="text-xs">
          What the application does, and for whom
        </Label>
        <Textarea
          id="blueprint-summary"
          rows={2}
          value={draft.summary}
          onChange={(event) =>
            setDraft({ ...draft, summary: event.target.value })
          }
        />
      </div>
      <section className="space-y-2" aria-label="Parts">
        <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Parts
        </h3>
        {draft.components.map((component) => (
          <PartEditor
            key={component.id}
            component={component}
            onChange={(next) =>
              setDraft(updateComponent(draft, component.id, next))
            }
            onRemove={() => setDraft(removeComponent(draft, component.id))}
          />
        ))}
        <div className="flex flex-col gap-2 sm:flex-row sm:items-end">
          <div className="sm:w-40">
            <Choice
              label="New part"
              value={newKind}
              options={KINDS}
              onChange={setNewKind}
            />
          </div>
          <div className="flex-1">
            <Field
              label="Name"
              value={newName}
              placeholder="Orders worker"
              onChange={setNewName}
            />
          </div>
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-8 gap-1.5"
            disabled={!newName.trim()}
            onClick={addPart}
          >
            <Plus className="size-4" />
            Add part
          </Button>
        </div>
      </section>
      <section className="space-y-2" aria-label="Flows">
        <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Flows
        </h3>
        {flows.map((flow, index) => (
          <FlowEditor
            key={index}
            flow={flow}
            parts={draft.components}
            onChange={(next) => setFlow(index, next)}
            onRemove={() => removeFlow(index)}
          />
        ))}
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="h-8 gap-1.5"
          disabled={draft.components.length < 2}
          onClick={addFlow}
        >
          <Plus className="size-4" />
          Add flow
        </Button>
      </section>
      <div className="space-y-1">
        <Label htmlFor="blueprint-change" className="text-xs">
          What you changed
        </Label>
        <Input
          id="blueprint-change"
          value={summary}
          placeholder="Moved sessions to Redis"
          onChange={(event) => setSummary(event.target.value)}
          className="h-8 text-xs"
        />
      </div>
      {problems.length > 0 && (
        <div
          role="alert"
          className="rounded-md border border-destructive/40 bg-destructive/5 p-3 text-xs"
        >
          <p className="font-medium text-destructive">
            The design was not saved:
          </p>
          <ul className="mt-1 list-disc space-y-0.5 pl-4">
            {problems.map((problem) => (
              <li key={problem}>{problem}</li>
            ))}
          </ul>
        </div>
      )}
      <div className="flex flex-wrap gap-2">
        <Button
          type="submit"
          size="sm"
          disabled={saving || !summary.trim() || draft.components.length === 0}
        >
          {saving && <Loader2 className="size-4 animate-spin" />}
          Save as a new version
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  )
}
