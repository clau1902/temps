// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { Button } from '@/components/ui/button'
import { useConsoleExtensions } from '@temps-sdk/console-kit'
import { LayoutDashboard } from 'lucide-react'
import { Link } from 'react-router'

/** Where "Back to console" lands. */
export const CONSOLE_HOME_PATH = '/projects'

/**
 * The only chrome around the full-screen assistant: the product mark, where
 * you are, and a way back. The console sidebar and header are deliberately not
 * rendered in this mode so the conversation is all there is to look at.
 */
export function AssistantTopBar() {
  const { logoIcon, logoText } = useConsoleExtensions()

  return (
    <header className="flex h-14 shrink-0 items-center justify-between gap-3 border-b px-4 sm:px-6">
      <div className="flex min-w-0 items-center gap-2">
        <Link
          to={CONSOLE_HOME_PATH}
          className="flex items-center gap-2 rounded-md"
          aria-label="Temps console"
          title="Back to console"
        >
          <span className="flex size-7 items-center justify-center">
            {logoIcon ?? (
              <img src="/svg/temps-icon.svg" alt="" className="size-full" />
            )}
          </span>
          <span className="hidden text-sm font-semibold sm:inline">
            {logoText ?? 'Temps'}
          </span>
        </Link>
        <span className="text-muted-foreground" aria-hidden="true">
          /
        </span>
        <h1 className="truncate text-sm font-medium">Assistant</h1>
      </div>
      <Button asChild variant="outline" size="sm" className="gap-1.5">
        <Link to={CONSOLE_HOME_PATH}>
          <LayoutDashboard className="size-4" />
          <span className="hidden sm:inline">Back to console</span>
          <span className="sm:hidden">Console</span>
        </Link>
      </Button>
    </header>
  )
}
