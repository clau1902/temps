// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/** Highest TCP port number. */
const MAX_PORT = 65535

/** A port typed by the user, or null when it is not one. */
export function parsePort(text: string): number | null {
  const trimmed = text.trim()
  if (!/^\d{1,5}$/.test(trimmed)) return null
  const port = Number(trimmed)
  return port >= 1 && port <= MAX_PORT ? port : null
}

/** The page for `port`, from the template the server gives (`{port}` is replaced). */
export function previewUrlFor(template: string, port: number): string {
  return template.replace('{port}', String(port))
}

/**
 * Whether the page can be shown inside the console. A browser refuses to load
 * an `http` page into an `https` one, so the user is sent to a new tab then.
 */
export function canEmbed(pageUrl: string, consoleProtocol: string): boolean {
  return !(consoleProtocol === 'https:' && pageUrl.startsWith('http:'))
}
