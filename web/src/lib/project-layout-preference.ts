// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Whether this browser opted into the v2 project layout (prototype).
 *
 * Opening any `/projects/:slug/v2` page opts in, so links the existing pages
 * build to classic project URLs land back in v2 instead of dropping the user
 * into the old layout. "Classic layout" in the v2 bar opts out.
 */
const STORAGE_KEY = 'temps:project-layout'

export function prefersProjectV2(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) === 'v2'
  } catch {
    return false
  }
}

export function setPrefersProjectV2(enabled: boolean) {
  try {
    if (enabled) window.localStorage.setItem(STORAGE_KEY, 'v2')
    else window.localStorage.removeItem(STORAGE_KEY)
  } catch {
    // Storage unavailable: the v2 URL still works, it just isn't sticky.
  }
}
