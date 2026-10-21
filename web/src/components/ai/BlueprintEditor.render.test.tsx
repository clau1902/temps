// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, expect, test } from 'bun:test'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import type { BlueprintDocument } from '@/api/client/types.gen'
import { installBrowserWindow } from '@/test/dom'
import { BlueprintEditor } from './BlueprintEditor'

installBrowserWindow()

afterEach(() => {
  cleanup()
})

const document: BlueprintDocument = {
  summary: 'Tasks for small teams',
  components: [
    {
      id: 'db',
      name: 'Task store',
      purpose: 'Rows',
      kind: 'data',
      engine: 'postgres',
      holds: ['tasks'],
    },
  ],
  flows: [],
}

test('the holds field shows what is typed and saves the list', () => {
  const saves: BlueprintDocument[] = []
  render(
    <BlueprintEditor
      initial={document}
      saving={false}
      problems={[]}
      onSave={(next) => saves.push(next)}
      onCancel={() => {}}
    />
  )
  const holds = screen.getByLabelText(
    'Holds (comma separated)'
  ) as HTMLInputElement
  expect(holds.value).toBe('tasks')

  // Each keystroke, as the browser reports it: the value is exactly the text.
  for (const typed of ['tasks,', 'tasks, ', 'tasks, u', 'tasks, users']) {
    fireEvent.change(holds, { target: { value: typed } })
    expect(holds.value).toBe(typed)
  }

  fireEvent.change(screen.getByLabelText('What you changed'), {
    target: { value: 'Store users too' },
  })
  fireEvent.click(screen.getByRole('button', { name: /Save as a new version/ }))
  expect(saves).toHaveLength(1)
  const store = saves[0]?.components.find((c) => c.id === 'db')
  expect(store?.holds).toEqual(['tasks', 'users'])
})
