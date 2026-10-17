// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  getChatSandboxOptions,
  getChatSandboxQueryKey,
  linkChatSandboxMutation,
  listSandboxesOptions,
  unlinkChatSandboxMutation,
} from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { cn } from '@/lib/utils'
import { getErrorMessage } from '@/utils/errorHandling'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Check, Loader2, SquareTerminal } from 'lucide-react'
import { useEffect, useState } from 'react'
import { Link } from 'react-router'
import { toast } from 'sonner'
import { linkableSandboxes, sandboxButtonLabel } from './chat-sandbox'

/** Shown instead of the example when the chat is about a project. */
const PROJECT_EXAMPLE =
  "A new sandbox starts as a clone of this project's repository, so the assistant can edit, test and run it there before anything is committed."

/** What the assistant could do with a shell, shown while nothing is linked. */
const EXAMPLE =
  'Clone a repository, unpack an uploaded archive and commit it, or start a server and check that it responds.'

function useChatSandbox(conversationPublicId: string) {
  const queryClient = useQueryClient()
  const status = useQuery(
    getChatSandboxOptions({ path: { public_id: conversationPublicId } })
  )
  const invalidate = () =>
    queryClient.invalidateQueries({
      queryKey: getChatSandboxQueryKey({
        path: { public_id: conversationPublicId },
      }),
    })
  const link = useMutation({
    ...linkChatSandboxMutation(),
    onSuccess: () => {
      toast.success('Sandbox linked to this chat')
      return invalidate()
    },
    onError: (error) =>
      toast.error('Could not link the sandbox', {
        description: getErrorMessage(error),
      }),
  })
  const unlink = useMutation({
    ...unlinkChatSandboxMutation(),
    onSuccess: () => {
      toast.success('Sandbox unlinked. It keeps running.')
      return invalidate()
    },
    onError: (error) =>
      toast.error('Could not unlink the sandbox', {
        description: getErrorMessage(error),
      }),
  })
  return { status, link, unlink }
}

function SandboxPicker({
  disabled,
  onPick,
}: {
  disabled: boolean
  onPick: (sandboxId: string) => void
}) {
  const sandboxes = useQuery(listSandboxesOptions({ query: { page_size: 50 } }))
  const options = linkableSandboxes(sandboxes.data?.sandboxes ?? [])

  if (sandboxes.isPending) return <Skeleton className="h-9 w-full" />
  if (options.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">
        You have no other sandboxes to link yet.
      </p>
    )
  }
  return (
    <Select disabled={disabled} onValueChange={onPick}>
      <SelectTrigger
        className="h-9 w-full"
        aria-label="Link one of my sandboxes"
      >
        <SelectValue placeholder="Link one of my sandboxes" />
      </SelectTrigger>
      <SelectContent>
        {options.map((sandbox) => (
          <SelectItem key={sandbox.id} value={sandbox.id}>
            <span className="truncate">{sandbox.name}</span>
            <span className="ml-2 text-xs text-muted-foreground">
              {sandbox.status}
            </span>
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

/**
 * Everything the user can do about a chat's sandbox, in one place: why it is
 * unavailable, create or pick one, or see and unlink the one that is linked.
 * Used by the composer-row popover and by the assistant's inline offer.
 */
export function SandboxLinkBody({
  conversationPublicId,
  fromProject = false,
  onDecline,
}: {
  conversationPublicId: string
  /** The chat is about a project: a new sandbox is cloned from its repository. */
  fromProject?: boolean
  /** Offered only where the user can say "not now", i.e. the inline offer. */
  onDecline?: () => void
}) {
  const { status, link, unlink } = useChatSandbox(conversationPublicId)

  if (status.isPending) return <Skeleton className="h-20 w-full" />
  if (status.isError || !status.data) {
    return (
      <p className="text-sm text-muted-foreground">
        The sandbox state of this chat could not be loaded.
      </p>
    )
  }
  const data = status.data

  if (!data.configured) {
    return (
      <div className="space-y-1 text-sm">
        <p className="font-medium">Sandbox unavailable</p>
        <p className="text-xs text-muted-foreground">
          {data.reason ?? 'This chat cannot use a sandbox.'}
        </p>
        {data.setup_path && (
          <Link
            to={data.setup_path}
            className="text-xs underline underline-offset-2"
          >
            Set it up
          </Link>
        )}
      </div>
    )
  }

  if (data.linked) {
    return (
      <div className="space-y-3 text-sm">
        <div className="min-w-0">
          <p className="font-medium">Sandbox linked</p>
          <p className="truncate text-xs">{data.linked.name}</p>
          <p className="truncate font-mono text-xs text-muted-foreground">
            {data.linked.sandbox_id}
          </p>
        </div>
        <p className="text-xs text-muted-foreground">
          The assistant can run commands and read and write files in this
          sandbox only. It holds no Git or platform credentials.
        </p>
        <div className="flex flex-wrap gap-2">
          <Button asChild variant="outline" size="sm">
            <Link to={`/sandboxes/${data.linked.sandbox_id}`}>
              Open sandbox
            </Link>
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={unlink.isPending}
            onClick={() =>
              unlink.mutate({ path: { public_id: conversationPublicId } })
            }
          >
            {unlink.isPending && <Loader2 className="size-4 animate-spin" />}
            Unlink
          </Button>
        </div>
      </div>
    )
  }

  const busy = link.isPending
  return (
    <div className="space-y-3 text-sm">
      <div>
        <p className="font-medium">Give this chat a sandbox</p>
        <p className="text-xs text-muted-foreground">
          {fromProject ? PROJECT_EXAMPLE : EXAMPLE}
        </p>
      </div>
      <Button
        type="button"
        size="sm"
        className="w-full"
        disabled={busy}
        onClick={() =>
          link.mutate({
            path: { public_id: conversationPublicId },
            body: { create: true },
          })
        }
      >
        {busy ? (
          <Loader2 className="size-4 animate-spin" />
        ) : (
          <SquareTerminal className="size-4" />
        )}
        Create a sandbox
      </Button>
      <SandboxPicker
        disabled={busy}
        onPick={(sandboxId) =>
          link.mutate({
            path: { public_id: conversationPublicId },
            body: { sandbox_id: sandboxId },
          })
        }
      />
      {onDecline && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="w-full"
          disabled={busy}
          onClick={onDecline}
        >
          Not now
        </Button>
      )}
    </div>
  )
}

/**
 * The chat's sandbox, always one click away. Unlinked, it explains what a
 * sandbox is for; unavailable, it says why instead of disappearing.
 */
export function ChatSandbox({
  conversationPublicId,
  fromProject = false,
  compact = false,
}: {
  conversationPublicId: string
  fromProject?: boolean
  /** Icon only on narrow screens, for crowded toolbars. */
  compact?: boolean
}) {
  const status = useQuery(
    getChatSandboxOptions({ path: { public_id: conversationPublicId } })
  )
  const label = sandboxButtonLabel(status.data)
  const linked = Boolean(status.data?.linked)

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="h-8 max-w-44 gap-1.5"
          title={
            linked
              ? 'The sandbox linked to this chat'
              : 'Give this chat a sandbox to run commands in'
          }
        >
          {linked ? (
            <Check className="size-4 shrink-0 text-emerald-600 dark:text-emerald-300" />
          ) : (
            <SquareTerminal className="size-4 shrink-0" />
          )}
          <span className={cn('truncate', compact && 'hidden sm:inline')}>
            {label}
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80">
        <SandboxLinkBody
          conversationPublicId={conversationPublicId}
          fromProject={fromProject}
        />
      </PopoverContent>
    </Popover>
  )
}

/**
 * The assistant created a sandbox itself. Refreshes the chat's sandbox status
 * once the tool has returned, so the Sandbox button shows the new link.
 */
export function SandboxStatusRefresher({
  conversationPublicId,
  done,
}: {
  conversationPublicId: string
  done: boolean
}) {
  const queryClient = useQueryClient()
  useEffect(() => {
    if (!done) return
    void queryClient.invalidateQueries({
      queryKey: getChatSandboxQueryKey({
        path: { public_id: conversationPublicId },
      }),
    })
  }, [done, conversationPublicId, queryClient])
  return null
}

/**
 * The sandbox control of a chat that has no conversation yet. A sandbox is
 * linked to a conversation, so it is shown, not hidden, and says what to do.
 */
export function ChatSandboxPending({ compact = false }: { compact?: boolean }) {
  return (
    <Button
      type="button"
      variant="outline"
      size="sm"
      className="h-8 max-w-44 gap-1.5"
      disabled
      title="Send your first message, then give this chat a sandbox to run commands in"
    >
      <SquareTerminal className="size-4 shrink-0" />
      <span className={cn('truncate', compact && 'hidden sm:inline')}>
        Sandbox
      </span>
    </Button>
  )
}

/**
 * The assistant asked for a sandbox. The user decides here; the model links
 * nothing itself.
 */
export function SandboxOfferCard({
  conversationPublicId,
  reason,
  fromProject = false,
}: {
  conversationPublicId: string
  reason: string | null
  fromProject?: boolean
}) {
  const [declined, setDeclined] = useState(false)
  const status = useQuery(
    getChatSandboxOptions({ path: { public_id: conversationPublicId } })
  )

  if (status.data?.linked) {
    return (
      <div className="flex items-center gap-2 rounded-lg border border-emerald-500/30 bg-emerald-500/5 px-3 py-2 text-xs">
        <Check className="size-4 shrink-0 text-emerald-600 dark:text-emerald-300" />
        <span className="min-w-0 truncate">
          Sandbox linked: {status.data.linked.name}
        </span>
      </div>
    )
  }
  if (declined) {
    return (
      <div className="rounded-lg border bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
        No sandbox for this chat. The assistant continues without a shell. You
        can link one any time from the Sandbox button.
      </div>
    )
  }
  return (
    <div className="min-w-0 space-y-3 rounded-lg border border-border bg-background p-3 sm:p-4">
      {reason && (
        <p className="text-xs text-muted-foreground">
          The assistant would like a sandbox to {reason}
        </p>
      )}
      <SandboxLinkBody
        conversationPublicId={conversationPublicId}
        fromProject={fromProject}
        onDecline={() => setDeclined(true)}
      />
    </div>
  )
}
