// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AssistantView } from './assistant-view'

/**
 * What the full-screen assistant keeps in its address, so a link or a reload
 * shows the same screen:
 * - `chat`: the open conversation.
 * - `view`: `chats` (list beside the chat) or `work` (chat beside its tabs).
 * - `tab`: the selected working tab (`made`, `preview`, `files`, `changes`,
 *   `terminal`, `project:<id>`, or a deployment as
 *   `<project id>:<deployment id>`).
 * - `panel=open`: the working tabs are open in the `chats` view.
 */
export const CHAT_PARAM = 'chat'
export const VIEW_PARAM = 'view'
export const TAB_PARAM = 'tab'
export const PANEL_PARAM = 'panel'

/**
 * The address as it is right now. A write made from an effect or a callback
 * must start from this, not from the `searchParams` of the render that created
 * it: a child's effect runs before its parent's, so a copy the parent keeps in
 * a ref is still the previous address then, and writing from it undoes the
 * change that caused the render (one click switching the view, the next one
 * finally sticking). `BrowserRouter` updates `window.location` synchronously
 * on every `setSearchParams`.
 */
export function liveSearchParams(): URLSearchParams {
  return new URLSearchParams(window.location.search)
}

/** The layout an address asks for, or null when it names none (or garbage). */
export function readViewParam(params: URLSearchParams): AssistantView | null {
  const raw = params.get(VIEW_PARAM)
  return raw === 'chats' || raw === 'work' ? raw : null
}

/** A copy of `params` with `name` set, or removed when `value` is null/empty. */
export function withParam(
  params: URLSearchParams,
  name: string,
  value: string | null
): URLSearchParams {
  const next = new URLSearchParams(params)
  if (value) next.set(name, value)
  else next.delete(name)
  return next
}

/** The working-tabs state belongs to one conversation; forget it with that chat. */
export function withoutChatScopedParams(
  params: URLSearchParams
): URLSearchParams {
  const next = new URLSearchParams(params)
  next.delete(TAB_PARAM)
  next.delete(PANEL_PARAM)
  return next
}

/** A copy of `params` that names no conversation (and none of its tabs). */
export function withoutChat(params: URLSearchParams): URLSearchParams {
  const next = withoutChatScopedParams(params)
  next.delete(CHAT_PARAM)
  return next
}

/**
 * The address after the open conversation or layout changed, or null when it
 * already says so. One pass, because two writes in the same commit would each
 * start from the same stale address and the second would undo the first.
 */
export function syncedPageParams(
  params: URLSearchParams,
  state: { chat: string | null; view: AssistantView }
): URLSearchParams | null {
  let next = new URLSearchParams(params)
  let changed = false
  if (state.chat && params.get(CHAT_PARAM) !== state.chat) {
    // Moving from one chat to another must not carry the old chat's tab along;
    // a first chat appearing in a bare address has nothing to drop anyway.
    if (params.has(CHAT_PARAM)) next = withoutChatScopedParams(next)
    next.set(CHAT_PARAM, state.chat)
    changed = true
  }
  if (params.get(VIEW_PARAM) !== state.view) {
    next.set(VIEW_PARAM, state.view)
    changed = true
  }
  return changed ? next : null
}
