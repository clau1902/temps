// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ConversationUsage } from '@/api/client'
import { getUserConversationUsageOptions } from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover'
import { Skeleton } from '@/components/ui/skeleton'
import { useQuery } from '@tanstack/react-query'
import { cn } from '@/lib/utils'
import { Coins } from 'lucide-react'
import {
  contextLabel,
  contextLevel,
  contextShare,
  formatSpend,
  formatTokens,
  spendLabel,
} from './chat-spend'

/** A chat is quiet between turns; this keeps the figure fresh while one runs. */
const REFRESH_MS = 15_000

/** What the model reads on every request: the whole chat so far. */
export function ContextMeter({
  context,
}: {
  context: NonNullable<ConversationUsage['context']>
}) {
  const share = contextShare(context)
  const level = contextLevel(context)
  return (
    <div className="space-y-1.5" aria-label="Context size">
      <div>
        <p className="font-medium">Context</p>
        <p className="text-xs text-muted-foreground">
          What the model reads on every request: the conversation so far, the
          instructions and the tools.
        </p>
      </div>
      <p className="text-xs tabular-nums">{contextLabel(context)}</p>
      {share !== null && (
        <div
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(share * 100)}
          className="h-1.5 w-full overflow-hidden rounded-full bg-muted"
        >
          <div
            className={cn(
              'h-full rounded-full',
              level === 'ok' && 'bg-foreground/60',
              level === 'high' && 'bg-amber-500',
              level === 'critical' && 'bg-red-500'
            )}
            style={{ width: `${Math.max(2, Math.round(share * 100))}%` }}
          />
        </div>
      )}
      {level !== 'ok' && (
        <p className="text-xs text-amber-600 dark:text-amber-300">
          A long chat costs more with every message, because all of it is sent
          again. Start a new chat for an unrelated task.
        </p>
      )}
    </div>
  )
}

function SpendBreakdown({ usage }: { usage: ConversationUsage }) {
  return (
    <div className="space-y-3 text-sm">
      {usage.context && <ContextMeter context={usage.context} />}
      <div>
        <p className="font-medium">What this chat has cost</p>
        <p className="text-xs text-muted-foreground">
          An estimate from each model&apos;s published price and the tokens it
          used.
        </p>
      </div>
      {usage.models.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          Nothing yet. Ask something and the spend appears here.
        </p>
      ) : (
        <ul className="space-y-1.5">
          {usage.models.map((model) => (
            <li
              key={model.model}
              className="flex items-start justify-between gap-3"
            >
              <span className="min-w-0">
                <span className="block truncate font-mono text-xs">
                  {model.model}
                </span>
                <span className="text-xs text-muted-foreground">
                  {model.requests}{' '}
                  {model.requests === 1 ? 'request' : 'requests'}
                  {' · '}
                  {formatTokens(model.input_tokens)} in,{' '}
                  {formatTokens(model.output_tokens)} out
                </span>
              </span>
              <span className="shrink-0 tabular-nums">
                {formatSpend(model.cost_microcents)}
              </span>
            </li>
          ))}
        </ul>
      )}
      {usage.unpriced_requests > 0 && (
        <p className="text-xs text-amber-600 dark:text-amber-300">
          {usage.unpriced_requests}{' '}
          {usage.unpriced_requests === 1 ? 'request has' : 'requests have'} no
          known price, so the total is a minimum.
        </p>
      )}
      {usage.byok_requests > 0 && (
        <p className="text-xs text-muted-foreground">
          {usage.byok_requests}{' '}
          {usage.byok_requests === 1 ? 'request used' : 'requests used'} your
          own provider key, which your provider bills you for directly.
        </p>
      )}
    </div>
  )
}

/**
 * What the chat has cost so far. Always shown, so the spend is never a thing
 * the user has to know to look for; the breakdown opens on click.
 */
export function ChatSpend({
  conversationPublicId,
}: {
  conversationPublicId: string
}) {
  const usage = useQuery({
    ...getUserConversationUsageOptions({
      path: { public_id: conversationPublicId },
    }),
    refetchInterval: REFRESH_MS,
    refetchIntervalInBackground: false,
  })

  if (usage.isPending) {
    return <Skeleton className="h-8 w-28" />
  }

  const spend = usage.data ? spendLabel(usage.data) : 'Spend unavailable'
  const context = usage.data?.context
  const label = context
    ? `${spend} · ${formatTokens(context.tokens)} ctx`
    : spend

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="h-8 gap-1.5 tabular-nums"
          title="See what this chat has cost and how much context it uses"
        >
          <Coins className="size-4" />
          {label}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80">
        {usage.data ? (
          <SpendBreakdown usage={usage.data} />
        ) : (
          <p className="text-sm text-muted-foreground">
            The spend for this chat could not be loaded. It will retry on its
            own.
          </p>
        )}
      </PopoverContent>
    </Popover>
  )
}
