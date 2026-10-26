// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// A deploy finished and nobody can reach the environment (ADR 068 D5): say
// so once, and offer one button per fix. The deploy itself succeeded; this
// never reads as a failure.

import { Globe } from 'lucide-react'
import { Link } from 'react-router'
import type { ReachabilityFix, ReachabilityWarning } from '@/api/client'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { CopyButton } from '@/components/ui/copy-button'

export function ReachabilityWarningBanner({
  warning,
  className,
}: {
  warning: ReachabilityWarning | null | undefined
  className?: string
}) {
  if (!warning) return null
  return (
    <Alert variant="warning" className={className}>
      <Globe className="h-4 w-4" />
      <AlertTitle>{warning.headline}</AlertTitle>
      <AlertDescription className="flex flex-col gap-3">
        <span>{warning.reason}</span>
        <div className="flex flex-col gap-2 sm:flex-row sm:flex-wrap">
          {warning.fixes.map((fix, index) => (
            <FixAction
              key={`${fix.kind}-${index}`}
              fix={fix}
              primary={index === 0}
            />
          ))}
        </div>
      </AlertDescription>
    </Alert>
  )
}

function FixAction({
  fix,
  primary,
}: {
  fix: ReachabilityFix
  primary: boolean
}) {
  if (fix.command) {
    return (
      <div className="flex min-w-0 items-center gap-2 rounded-md border bg-background px-2 py-1">
        <span className="text-sm">{fix.label}:</span>
        <code className="truncate font-mono text-xs">{fix.command}</code>
        <CopyButton
          value={fix.command}
          label={`Copy ${fix.command}`}
          className="shrink-0"
        />
      </div>
    )
  }
  if (!fix.href) return null
  return (
    <Button
      size="sm"
      variant={primary ? 'default' : 'outline'}
      className="w-full sm:w-auto"
      asChild
    >
      <Link to={fix.href}>{fix.label}</Link>
    </Button>
  )
}
