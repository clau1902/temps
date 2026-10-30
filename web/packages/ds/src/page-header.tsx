// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useContext, type ReactNode } from 'react'
import { cn } from './lib/cn'
import {
  InsidePageContainerContext,
  PageNamedByChromeContext,
} from './page-context'

// Promoted from web/src/components/layout/PageContainer.tsx (it was already
// solid — full-width shell, responsive padding). web/src's own
// PageContainer.tsx now re-exports this module instead of defining its own
// copy, so there is exactly one implementation. `PageHeader` is extended
// here with the optional `verdict` slot the record recipe needs (title ->
// verdict -> facts).

/**
 * How wide a page's content may grow:
 * - `form`: settings and other forms, a centered column of about 1024px, so
 *   fields and their explanations stay readable.
 * - `full`: everything else (lists, tables, dashboards, logs), edge to edge
 *   with the gutters, so the page lines up with the bar above it.
 */
export type PageWidth = 'form' | 'full'

const PAGE_WIDTH_CLASS: Record<PageWidth, string> = {
  form: 'mx-auto max-w-5xl',
  full: '',
}

export interface PageContainerProps {
  /** Extra classes on the padded outer wrapper. */
  className?: string
  /** Extra classes on the content wrapper (e.g. spacing). */
  innerClassName?: string
  /**
   * How wide the content may grow. Defaults to `full`, the original
   * behaviour; the header and tabs belong inside, so they line up with it.
   */
  width?: PageWidth
  children: ReactNode
}

/**
 * Standard page shell — owns responsive horizontal padding and width.
 *
 * Only the outermost one does: a page template rendered inside a route shell
 * that already has a `PageContainer` keeps its vertical rhythm but adds no
 * second gutter or width cap, so it lines up with the header above it.
 */
export function PageContainer({
  className,
  innerClassName,
  width = 'full',
  children,
}: PageContainerProps) {
  const nested = useContext(InsidePageContainerContext)
  if (nested) {
    return (
      <div
        data-page-container-nested
        className={cn('w-full min-w-0 space-y-6', className, innerClassName)}
      >
        {children}
      </div>
    )
  }
  return (
    <div
      data-page-container
      data-page-width={width}
      className={cn('w-full px-4 py-6 sm:px-6 lg:px-8', className)}
    >
      <div
        className={cn(
          'w-full min-w-0 space-y-6',
          PAGE_WIDTH_CLASS[width],
          innerClassName
        )}
      >
        <InsidePageContainerContext.Provider value={true}>
          {children}
        </InsidePageContainerContext.Provider>
      </div>
    </div>
  )
}

export interface PageHeaderProps {
  /** Use h2 for a nested example or section; page titles remain h1. */
  headingLevel?: 'h1' | 'h2'
  title: ReactNode
  description?: ReactNode
  /**
   * The record recipe's second beat: a `Status` or similar one-glance
   * verdict, rendered directly under the title before any facts. Omit on
   * list/settings pages — only record (`Detail`) pages have a verdict.
   */
  verdict?: ReactNode
  actions?: ReactNode
  className?: string
}

export function PageHeader({
  headingLevel: Heading = 'h1',
  title,
  description,
  verdict,
  actions,
  className,
}: PageHeaderProps) {
  // A tab strip above already names the page: keep the h1 for screen readers
  // and outlines, but do not print the same words twice.
  const namedByChrome = useContext(PageNamedByChromeContext) && Heading === 'h1'
  if (namedByChrome && !description && !verdict && !actions) {
    return <Heading className="sr-only">{title}</Heading>
  }
  return (
    <div
      data-page-header
      className={cn(
        'flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between',
        className
      )}
    >
      <div className="min-w-0">
        <div
          className={cn(
            'flex flex-wrap items-center gap-2',
            namedByChrome && !verdict && 'contents'
          )}
        >
          <Heading
            className={
              namedByChrome
                ? 'sr-only'
                : 'text-2xl font-semibold tracking-tight'
            }
          >
            {title}
          </Heading>
          {verdict}
        </div>
        {description ? (
          <p
            className={cn(
              'text-sm text-muted-foreground',
              !namedByChrome && 'mt-1'
            )}
          >
            {description}
          </p>
        ) : null}
      </div>
      {actions ? (
        <div className="flex min-w-0 flex-wrap items-center gap-2 sm:justify-end">
          {actions}
        </div>
      ) : null}
    </div>
  )
}
