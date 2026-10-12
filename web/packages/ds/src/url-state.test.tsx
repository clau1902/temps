// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { MemoryRouter, useLocation } from 'react-router'
import { useUrlBoundText, useUrlState } from './url-state'

import {
  act,
  cleanup,
  fireEvent,
  render,
  waitFor,
} from '@testing-library/react'
import { useBrowserWindow } from '../../../src/test/dom'

useBrowserWindow()
afterEach(cleanup)

function SearchBox() {
  const { get, patch } = useUrlState<'q' | 'page' | 'tab'>()
  const [text, setText] = useUrlBoundText(get('q') ?? '', (value) =>
    // The same two-step update the list pages used to make: search, then page.
    {
      patch({ q: value || undefined })
      patch({ page: undefined })
    }
  )
  const location = useLocation()
  return (
    <div>
      <input
        aria-label="search"
        value={text}
        onChange={(event) => setText(event.target.value)}
      />
      <output data-testid="search">{location.search}</output>
    </div>
  )
}

test('typing a multi-character term into a URL-bound search keeps the full value', async () => {
  const view = render(
    <MemoryRouter initialEntries={['/list?tab=backups&page=3']}>
      <SearchBox />
    </MemoryRouter>
  )
  const input = view.getByLabelText('search') as HTMLInputElement
  let typed = ''
  for (const char of 'qa-ui-qj14a') {
    typed += char
    fireEvent.change(input, { target: { value: typed } })
  }
  expect(input.value).toBe('qa-ui-qj14a')
  // The search and the page reset both landed, and untouched params survived.
  await waitFor(() => {
    const params = new URLSearchParams(
      view.getByTestId('search').textContent ?? ''
    )
    expect(params.get('q')).toBe('qa-ui-qj14a')
    expect(params.has('page')).toBe(false)
    expect(params.get('tab')).toBe('backups')
  })
  expect(input.value).toBe('qa-ui-qj14a')
  view.unmount()
})

test('patch applies every key of one call in a single update', async () => {
  function Patcher() {
    const { patch } = useUrlState<'q' | 'page'>()
    const location = useLocation()
    return (
      <div>
        <button onClick={() => patch({ q: 'abc', page: 2 })}>go</button>
        <output data-testid="search">{location.search}</output>
      </div>
    )
  }
  const view = render(
    <MemoryRouter initialEntries={['/list']}>
      <Patcher />
    </MemoryRouter>
  )
  await act(async () => {
    fireEvent.click(view.getByText('go'))
  })
  const params = new URLSearchParams(
    view.getByTestId('search').textContent ?? ''
  )
  expect(params.get('q')).toBe('abc')
  expect(params.get('page')).toBe('2')
  view.unmount()
})

test('an external URL change replaces the text in the box', async () => {
  function Linked() {
    const { get, patch } = useUrlState<'q'>()
    const [text] = useUrlBoundText(get('q') ?? '', (value) =>
      patch({ q: value })
    )
    return (
      <div>
        <input aria-label="search" value={text} readOnly />
        <button onClick={() => patch({ q: 'from-link' })}>apply</button>
      </div>
    )
  }
  const view = render(
    <MemoryRouter initialEntries={['/list']}>
      <Linked />
    </MemoryRouter>
  )
  await act(async () => {
    fireEvent.click(view.getByText('apply'))
  })
  expect((view.getByLabelText('search') as HTMLInputElement).value).toBe(
    'from-link'
  )
  view.unmount()
})
