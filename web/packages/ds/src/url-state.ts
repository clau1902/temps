// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useSearchParams } from 'react-router'

export type UrlStateValue = string | number | boolean | undefined | null

/**
 * `useSearchParams` whose updater composes across calls.
 *
 * React Router hands a functional updater the params from the *render* that
 * created `setParams`, not the latest navigation. Two updates in one event
 * (`q` then `page`, or an event handler plus a canonicalising effect) would
 * both start from the same stale params and the second would overwrite the
 * first. `pending` holds the result of the most recent update until the router
 * renders it, so consecutive updates build on each other instead.
 */
export function useComposedSearchParams() {
  const [params, setParams] = useSearchParams()
  const pending = useRef<URLSearchParams | null>(null)
  useEffect(() => {
    pending.current = null
  }, [params])

  const update = useCallback(
    (
      transform: (current: URLSearchParams) => URLSearchParams,
      options?: { replace?: boolean }
    ) => {
      setParams(
        (current) => {
          const next = transform(
            new URLSearchParams(pending.current ?? current)
          )
          pending.current = next
          return next
        },
        { replace: options?.replace ?? true }
      )
    },
    [setParams]
  )

  return [params, update] as const
}

/**
 * Generalizes the URL-state pattern already used by `useGlobalView`
 * (web/src/hooks/useGlobalView.ts) — read query params as a typed object,
 * patch them immutably, never lose params you didn't touch. Where
 * `useGlobalView` is specific to the observability time-window filters, this
 * is the general-purpose version: any list, detail, or settings page whose
 * filters/tab/page number should survive a refresh or a shared link uses
 * this instead of local `useState`. See RULES.md: "the URL is the state".
 *
 * `undefined`/`null` in a patch deletes the key; everything else is
 * stringified. Reads are always strings — parse at the call site (the
 * `fmt.ts` helpers or a small local `Number(...)`/enum guard), keeping this
 * hook free of per-page parsing logic.
 */
export function useUrlState<K extends string = string>() {
  const [params, update] = useComposedSearchParams()

  const get = useCallback((key: K) => params.get(key), [params])

  const getAll = useCallback((key: K) => params.getAll(key), [params])

  const state = useMemo(() => {
    const out: Record<string, string> = {}
    for (const [key, value] of params.entries()) out[key] = value
    return out as Record<K, string | undefined>
  }, [params])

  // All keys in `values` land in one navigation, so a search term and its page
  // reset are patched together rather than as two racing updates.
  const patch = useCallback(
    (
      values: Partial<Record<K, UrlStateValue>>,
      options?: { replace?: boolean }
    ) => {
      update((next) => {
        for (const [key, value] of Object.entries(values)) {
          if (value === undefined || value === null || value === '') {
            next.delete(key)
          } else {
            next.set(key, String(value))
          }
        }
        return next
      }, options)
    },
    [update]
  )

  const clear = useCallback(
    (keys: K[], options?: { replace?: boolean }) => {
      update((next) => {
        for (const key of keys) next.delete(key)
        return next
      }, options)
    },
    [update]
  )

  return { params, state, get, getAll, patch, clear }
}

export type UrlState = ReturnType<typeof useUrlState>

/**
 * A text box whose value lives in the URL, without dropping keystrokes.
 *
 * Reading the box straight from the URL is not safe: the router applies
 * navigations in a transition, so a controlled input is briefly reset to the
 * previous URL value and a character typed in that window is lost. This keeps
 * the text in local state (so the box always shows exactly what was typed) and
 * mirrors every change to the URL through `commit`.
 *
 * When the URL changes for any reason other than our own writes (back/forward,
 * a "clear filters" link) the new value is adopted. Writes still in flight are
 * remembered so a late echo of an older keystroke never overwrites newer text.
 */
export function useUrlBoundText(
  urlValue: string,
  commit: (value: string) => void
): [string, (value: string) => void] {
  const [text, setText] = useState(urlValue)
  const inFlight = useRef<string[]>([])

  useEffect(() => {
    const index = inFlight.current.lastIndexOf(urlValue)
    if (index >= 0) {
      inFlight.current = inFlight.current.slice(index + 1)
      return
    }
    inFlight.current = []
    setText(urlValue)
  }, [urlValue])

  const update = useCallback(
    (value: string) => {
      setText(value)
      inFlight.current.push(value)
      commit(value)
    },
    [commit]
  )

  return [text, update]
}
