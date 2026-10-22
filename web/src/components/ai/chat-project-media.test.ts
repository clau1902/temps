// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { expect, test } from 'bun:test'
import {
  chatProjectIds,
  MEDIA_BATCH_SIZE,
  mediaBatches,
  mediaProjectIds,
} from './chat-project-media'

test('chats in the same project ask about it once, in a stable order', () => {
  expect(chatProjectIds([7, null, 3, 7, undefined, 3, 12])).toEqual([3, 7, 12])
})

test('only readable projects Temps deploys are asked about', () => {
  expect(
    mediaProjectIds([
      { id: 4, source_type: 'git' },
      { id: 2, source_type: 'external' },
      undefined,
      null,
      { id: 1 },
    ])
  ).toEqual([1, 4])
})

test('every listed project goes out in as few requests as the endpoint allows', () => {
  const ids = Array.from({ length: 2 * MEDIA_BATCH_SIZE + 5 }, (_, i) => i + 1)
  const batches = mediaBatches(ids)
  expect(batches.map((batch) => batch.length)).toEqual([
    MEDIA_BATCH_SIZE,
    MEDIA_BATCH_SIZE,
    5,
  ])
  expect(batches.flat()).toEqual(ids)
  expect(mediaBatches([])).toEqual([])
})
