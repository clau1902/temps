// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterAll } from 'bun:test'
import { GlobalRegistrator } from '@happy-dom/global-registrator'

/**
 * Registers a browser-like DOM for the duration of one test file.
 *
 * `bun test` runs every file in a single process, and most of this repo's
 * tests are server-render assertions that must not see a `window`. Interactive
 * tests (typing, clicking) therefore opt in per file and tear the globals down
 * again. Call `useDom()` at the top level of the test file, then load Testing
 * Library with a top-level `await import('@testing-library/react')` -- it must
 * not be imported statically, because it inspects `document` when evaluated.
 * Unmount what you render; auto-cleanup is only registered for the first file
 * that imports the package.
 */
export function useDom() {
  if (!GlobalRegistrator.isRegistered) {
    GlobalRegistrator.register({ url: 'http://localhost/' })
  }
  ;(
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true
  afterAll(async () => {
    if (GlobalRegistrator.isRegistered) await GlobalRegistrator.unregister()
  })
}
