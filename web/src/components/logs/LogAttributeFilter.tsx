// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useState, type FormEvent } from 'react'
import { Button, Field, FormErrors } from '@temps-sdk/ds'
import {
  Input,
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@temps-sdk/ui'
import type { LogAttributeOperator, LogAttributeType } from '@/api/client'

export interface LogAttributePredicate {
  key: string
  value: string
  type?: LogAttributeType
  operator: LogAttributeOperator
}
const ANY = '__any__'
const TYPES: LogAttributeType[] = [
  'string',
  'boolean',
  'integer',
  'double',
  'array',
  'object',
  'bytes',
]
const OPERATORS: { value: LogAttributeOperator; label: string }[] = [
  { value: 'eq', label: 'Equals' },
  { value: 'ne', label: 'Does not equal' },
  { value: 'gt', label: 'Greater than' },
  { value: 'gte', label: 'At least' },
  { value: 'lt', label: 'Less than' },
  { value: 'lte', label: 'At most' },
]

/** Form section inside the existing log explorer; requests use its native SDK. */
export function LogAttributeFilter({
  value,
  onApply,
  pending,
}: {
  value?: LogAttributePredicate
  onApply: (value?: LogAttributePredicate) => void
  pending: boolean
}) {
  const [key, setKey] = useState(value?.key ?? '')
  const [text, setText] = useState(value?.value ?? '')
  const [type, setType] = useState<LogAttributeType | typeof ANY>(
    value?.type ?? ANY
  )
  const [operator, setOperator] = useState<LogAttributeOperator>(
    value?.operator ?? 'eq'
  )
  const [errors, setErrors] = useState<Record<string, string | undefined>>({})
  const numeric = type === 'integer' || type === 'double'
  function submit(event: FormEvent) {
    event.preventDefault()
    const next: Record<string, string | undefined> = {}
    if (
      !key ||
      new TextEncoder().encode(key).length > 128 ||
      key.includes('\0')
    )
      next.Attribute = 'Enter an attribute key of 1–128 bytes.'
    if (new TextEncoder().encode(text).length > 16384 || text.includes('\0'))
      next.Value = 'Enter a value of at most 16 KiB without NUL characters.'
    if (type === 'integer') {
      try {
        if (
          !/^[+-]?\d+$/.test(text) ||
          BigInt(text) < -(2n ** 63n) ||
          BigInt(text) >= 2n ** 63n
        )
          next.Value = 'Enter a signed 64-bit integer.'
      } catch {
        next.Value = 'Enter a signed 64-bit integer.'
      }
    }
    if (
      type === 'double' &&
      (!/^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$/.test(text) ||
        !Number.isFinite(Number(text)))
    )
      next.Value = 'Enter a finite number.'
    if (type === 'boolean' && text !== 'true' && text !== 'false')
      next.Value = 'Enter true or false.'
    setErrors(next)
    if (Object.keys(next).length) return
    // Keep numeric input as text. Number()/JSON numeric serialization would
    // round large int64 values before they reach the native query service.
    onApply({
      key,
      value: text,
      type: type === ANY ? undefined : type,
      operator,
    })
  }
  return (
    <form
      className="mt-3 space-y-3 border-t pt-3"
      onSubmit={submit}
      aria-label="Filter log attributes"
    >
      <p className="text-xs text-muted-foreground">
        Filter an attribute by value and declared type. Older logs without
        native type metadata can use “Any type”.
      </p>
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Field label="Attribute key" error={errors.Attribute}>
          {(props) => (
            <Input
              {...props}
              value={key}
              onChange={(event) => setKey(event.target.value)}
              placeholder="order.total"
            />
          )}
        </Field>
        <Field label="Attribute type" optional>
          {(props) => (
            <Select
              value={type}
              onValueChange={(value) => {
                setType(value as LogAttributeType | typeof ANY)
                if (value !== 'integer' && value !== 'double') setOperator('eq')
              }}
            >
              <SelectTrigger {...props}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={ANY}>Any type</SelectItem>
                {TYPES.map((value) => (
                  <SelectItem key={value} value={value}>
                    {value}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
        </Field>
        <Field label="Comparison">
          {(props) => (
            <Select
              value={operator}
              onValueChange={(value) =>
                setOperator(value as LogAttributeOperator)
              }
            >
              <SelectTrigger {...props}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {OPERATORS.filter(
                  (item) =>
                    numeric || item.value === 'eq' || item.value === 'ne'
                ).map((item) => (
                  <SelectItem key={item.value} value={item.value}>
                    {item.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
        </Field>
        <Field label="Attribute value" error={errors.Value}>
          {(props) => (
            <Input
              {...props}
              value={text}
              onChange={(event) => setText(event.target.value)}
              placeholder={type === 'boolean' ? 'true' : 'Value'}
            />
          )}
        </Field>
      </div>
      <FormErrors errors={errors} />
      <div className="flex flex-wrap gap-2">
        <Button
          type="submit"
          variant="outline"
          busy={pending}
          busyLabel="Applying…"
        >
          Apply attribute filter
        </Button>
        {value && (
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setKey('')
              setText('')
              setType(ANY)
              setOperator('eq')
              setErrors({})
              onApply(undefined)
            }}
          >
            Clear attribute filter
          </Button>
        )}
      </div>
    </form>
  )
}
