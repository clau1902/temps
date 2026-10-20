// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  getChatSandboxQueryKey,
  getChatWorkspaceQueryKey,
  leaveChatWorkspaceMutation,
  listApplicationsOptions,
  setChatWorkspaceMutation,
} from '@/api/client/@tanstack/react-query.gen'
import type { ChatWorkspaceResponse } from '@/api/client/types.gen'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover'
import { Skeleton } from '@/components/ui/skeleton'
import { TimeAgo } from '@/components/utils/TimeAgo'
import { cn } from '@/lib/utils'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Boxes, Loader2, LogOut, MessageSquare, Plus } from 'lucide-react'
import { useState } from 'react'
import { toast } from 'sonner'
import { invalidateChatResources } from './chat-resources'
import {
  invalidateBlueprint,
  openChat,
  useChatBlueprint,
} from './chat-blueprint'

/**
 * The application (workspace) this chat belongs to. The chats of one
 * application share its sandbox, its projects and its architecture, so a new
 * chat about the same app picks up where the others left off.
 */
export function WorkspaceChip({
  conversationPublicId,
}: {
  conversationPublicId: string
}) {
  const queryClient = useQueryClient()
  const { workspace } = useChatBlueprint(conversationPublicId)
  const [open, setOpen] = useState(false)
  const [name, setName] = useState('')
  const applications = useQuery({
    ...listApplicationsOptions({ query: { page_size: 50 } }),
    enabled: open,
  })
  const settled = (data: ChatWorkspaceResponse, message: string) => {
    queryClient.setQueryData(
      getChatWorkspaceQueryKey({ path: { public_id: conversationPublicId } }),
      data
    )
    if (data.workspace) {
      invalidateBlueprint(queryClient, data.workspace.public_id)
    }
    // The chat now shares the application's sandbox, or has its own again.
    void queryClient.invalidateQueries({
      queryKey: getChatSandboxQueryKey({
        path: { public_id: conversationPublicId },
      }),
    })
    void invalidateChatResources(queryClient)
    toast.success(message)
    setName('')
    setOpen(false)
  }
  const join = useMutation({
    ...setChatWorkspaceMutation(),
    meta: { errorTitle: 'Could not move the chat' },
    onSuccess: (data) =>
      settled(
        data,
        data.workspace
          ? `This chat is now part of ${data.workspace.name}`
          : 'Moved'
      ),
  })
  const leave = useMutation({
    ...leaveChatWorkspaceMutation(),
    meta: { errorTitle: 'Could not take the chat out of the application' },
    onSuccess: (data) => settled(data, 'This chat is on its own again'),
  })
  const path = { public_id: conversationPublicId }
  const current = workspace.data?.workspace ?? null
  const busy = join.isPending || leave.isPending

  if (workspace.isPending) return <Skeleton className="h-8 w-28" />
  // A project chat works on its project alone.
  if (!workspace.data?.joinable) return null

  const others = (applications.data ?? []).filter(
    (application) => application.public_id !== current?.public_id
  )
  const siblings = (workspace.data.chats ?? []).filter(
    (chat) => chat.public_id !== conversationPublicId
  )
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="h-8 max-w-44 gap-1.5"
          title={
            current
              ? `Part of the application ${current.name}`
              : 'Group this chat with others about the same application'
          }
        >
          <Boxes className="size-4 shrink-0" />
          <span className="hidden truncate sm:inline">
            {current?.name ?? 'Application'}
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 space-y-3 p-3">
        <div>
          <p className="text-sm font-medium">
            {current ? current.name : 'Not part of an application'}
          </p>
          <p className="text-xs text-muted-foreground">
            Chats of one application share its sandbox, its projects and its
            architecture.
          </p>
        </div>
        {current && siblings.length > 0 && (
          <section aria-label="Other chats" className="space-y-1">
            <p className="text-xs font-medium text-muted-foreground">
              Other chats
            </p>
            <ul className="max-h-40 overflow-y-auto">
              {siblings.map((chat) => (
                <li key={chat.public_id}>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs hover:bg-muted"
                    onClick={() => {
                      setOpen(false)
                      openChat(chat.public_id)
                    }}
                  >
                    <MessageSquare className="size-3.5 shrink-0 text-muted-foreground" />
                    <span className="min-w-0 flex-1 truncate">
                      {chat.title ?? 'Untitled chat'}
                    </span>
                    <span className="shrink-0 text-muted-foreground">
                      <TimeAgo date={chat.last_activity_at} />
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
        <form
          className="flex gap-2"
          onSubmit={(event) => {
            event.preventDefault()
            if (!name.trim()) return
            join.mutate({
              path,
              body: { create: { name: name.trim() }, keep_sandbox: 'chat' },
            })
          }}
        >
          <Input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="New application name"
            aria-label="New application name"
            className="h-8 text-xs"
          />
          <Button
            type="submit"
            size="sm"
            variant="outline"
            className="h-8 shrink-0 gap-1"
            disabled={busy || !name.trim()}
          >
            {join.isPending ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <Plus className="size-3.5" />
            )}
            Create
          </Button>
        </form>
        <section aria-label="Move to an application" className="space-y-1">
          <p className="text-xs font-medium text-muted-foreground">
            {current ? 'Move to another application' : 'Add to an application'}
          </p>
          {applications.isPending ? (
            <div className="space-y-1">
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-2/3" />
            </div>
          ) : others.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              No other applications yet.
            </p>
          ) : (
            <ul className="max-h-40 overflow-y-auto">
              {others.map((application) => (
                <li key={application.public_id}>
                  <button
                    type="button"
                    disabled={busy}
                    className={cn(
                      'flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs hover:bg-muted',
                      busy && 'opacity-60'
                    )}
                    onClick={() =>
                      join.mutate({
                        path,
                        body: {
                          application_id: application.public_id,
                          keep_sandbox: 'workspace',
                        },
                      })
                    }
                  >
                    <Boxes className="size-3.5 shrink-0 text-muted-foreground" />
                    <span className="min-w-0 flex-1 truncate">
                      {application.name}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
        {current && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-8 w-full justify-start gap-1.5 text-xs"
            disabled={busy}
            onClick={() => leave.mutate({ path })}
          >
            {leave.isPending ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <LogOut className="size-3.5" />
            )}
            Take this chat out of {current.name}
          </Button>
        )}
      </PopoverContent>
    </Popover>
  )
}
