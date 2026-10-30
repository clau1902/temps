// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { createContext, useContext } from 'react'

/**
 * True inside a `PageContainer`. A page template (`Ledger`, `CardGrid`,
 * `Detail`) rendered by a route shell that already owns the page column would
 * otherwise add its gutters a second time and sit indented from the header.
 */
export const InsidePageContainerContext = createContext(false)

/**
 * True when the chrome around the page already names it, e.g. a section's
 * tab strip with this page's tab selected. The page's own `h1` then stays for
 * assistive technology but is not shown a second time.
 */
export const PageNamedByChromeContext = createContext(false)

/** Whether the surrounding chrome already shows this page's title. */
export function usePageNamedByChrome(): boolean {
  return useContext(PageNamedByChromeContext)
}
