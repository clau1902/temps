// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  createAssistantConversation,
  createProviderKey,
  type ConversationResponse,
} from '@/api/client'
import {
  getAiProviderStatusOptions,
  getAiProviderStatusQueryKey,
  listProviderKeysQueryKey,
} from '@/api/client/@tanstack/react-query.gen'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Skeleton } from '@/components/ui/skeleton'
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { cn } from '@/lib/utils'
import { getErrorMessage } from '@/utils/errorHandling'
import { zodResolver } from '@hookform/resolvers/zod'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  ArrowRight,
  Cloud,
  KeyRound,
  Loader2,
  Send,
  Sparkles,
} from 'lucide-react'
import { useState } from 'react'
import { useForm } from 'react-hook-form'
import { Link } from 'react-router'
import { toast } from 'sonner'
import { z } from 'zod'
import {
  ASSISTANT_CONTEXT_TYPE,
  ASSISTANT_EXAMPLE_PROMPTS,
  assistantReadiness,
  setupPathFromProblem,
} from './assistant-state'
import type { ChatDetailLevel } from './chat-detail-level'
import { DebugChatPanel } from './DebugChatPanel'

const keySchema = z.object({
  api_key: z.string().trim().min(1, 'Paste your OpenRouter API key'),
})
type KeyForm = z.infer<typeof keySchema>

/**
 * What the assistant does and exactly what is missing, with the fix on the
 * same screen. Shown in place of the chat whenever no AI provider exists, so
 * the feature is discoverable instead of absent.
 *
 * Pasting an OpenRouter key here is the whole setup: it is verified and stored
 * encrypted by the same endpoint the AI Gateway page uses, and never shown
 * again.
 */
export function AssistantOnboarding({
  reason,
  setupPath,
  onConfigured,
  onNavigate,
  cloudPath,
  roomy,
}: {
  reason: string
  setupPath: string
  /** Full-screen sizing: larger type and controls, content centred. */
  roomy?: boolean
  /** Called once the key is saved and the assistant can start. */
  onConfigured?: () => void
  /** Called when a link is followed, e.g. to close the dock on small screens. */
  onNavigate?: () => void
  /** Secondary route for operators who use managed AI instead. */
  cloudPath?: string
}) {
  const queryClient = useQueryClient()
  const form = useForm<KeyForm>({
    resolver: zodResolver(keySchema),
    defaultValues: { api_key: '' },
  })
  const save = useMutation({
    mutationFn: async (values: KeyForm) =>
      createProviderKey({
        body: {
          provider: 'openrouter',
          display_name: 'OpenRouter',
          api_key: values.api_key,
        },
        throwOnError: true,
      }),
    onSuccess: async () => {
      form.reset()
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: getAiProviderStatusQueryKey(),
        }),
        queryClient.invalidateQueries({ queryKey: listProviderKeysQueryKey() }),
        queryClient.invalidateQueries({ queryKey: ['providerKeys'] }),
      ])
      toast.success('OpenRouter key saved')
      onConfigured?.()
    },
  })

  return (
    <div
      className={cn(
        'flex h-full min-h-0 flex-col gap-5 overflow-y-auto p-5',
        roomy && 'justify-center gap-8 p-6'
      )}
    >
      <div className={cn('space-y-2', roomy && 'space-y-4')}>
        <Badge
          variant="outline"
          className={cn(
            'gap-1.5 whitespace-normal font-normal',
            roomy && 'px-3 py-1 text-sm'
          )}
        >
          <KeyRound
            className={cn('size-3 shrink-0', roomy && 'size-4')}
            aria-hidden="true"
          />
          {reason}
        </Badge>
        <h2
          className={cn(
            'text-xl font-semibold tracking-tight',
            roomy && 'text-4xl'
          )}
        >
          Manage Temps by chat
        </h2>
        <p
          className={cn(
            'text-sm leading-6 text-muted-foreground',
            roomy && 'text-lg leading-8'
          )}
        >
          Ask questions about your projects, deployments and settings in plain
          language. The assistant uses the same API as this console, with your
          own permissions.
        </p>
      </div>

      <AssistantExample roomy={roomy} />

      <form
        noValidate
        onSubmit={form.handleSubmit((values) => save.mutate(values))}
        className={cn('space-y-2', roomy && 'space-y-3')}
      >
        <label
          htmlFor="assistant-openrouter-key"
          className={cn('text-sm font-medium', roomy && 'text-base')}
        >
          OpenRouter API key
        </label>
        <div className="flex gap-2">
          <Input
            className={cn(roomy && 'h-12 text-base md:text-base')}
            id="assistant-openrouter-key"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder="sk-or-…"
            disabled={save.isPending}
            aria-invalid={form.formState.errors.api_key ? true : undefined}
            {...form.register('api_key')}
          />
          <Button
            type="submit"
            disabled={save.isPending}
            className={cn(roomy && 'h-12 px-6 text-base')}
          >
            {save.isPending ? (
              <Loader2 className="size-4 animate-spin" aria-hidden="true" />
            ) : null}
            Save key
          </Button>
        </div>
        {form.formState.errors.api_key ? (
          <p className="text-sm text-destructive" role="alert">
            {form.formState.errors.api_key.message}
          </p>
        ) : null}
        {save.isError ? (
          <p className="text-sm text-destructive" role="alert">
            {getErrorMessage(
              save.error,
              'OpenRouter rejected the key. Check it and try again.'
            )}
          </p>
        ) : null}
        <p
          className={cn(
            'text-xs leading-5 text-muted-foreground',
            roomy && 'text-sm leading-6'
          )}
        >
          The key is checked with OpenRouter, then stored encrypted. It is never
          shown again.{' '}
          <a
            className="underline underline-offset-2"
            href="https://openrouter.ai/keys"
            target="_blank"
            rel="noreferrer"
          >
            Get a key
          </a>
        </p>
      </form>

      <div className={cn('grid gap-2', roomy && 'gap-3')}>
        <Button
          asChild
          variant="outline"
          className={cn('w-full justify-between', roomy && 'h-12 text-base')}
        >
          <Link to={setupPath} onClick={onNavigate}>
            Open AI provider settings <ArrowRight className="size-4" />
          </Link>
        </Button>
        {cloudPath ? (
          <Button
            asChild
            variant="ghost"
            className={cn(
              'w-full justify-between text-muted-foreground',
              roomy && 'h-12 text-base'
            )}
          >
            <Link to={cloudPath} onClick={onNavigate}>
              <span className="flex items-center gap-2">
                <Cloud className="size-4" aria-hidden="true" />
                Use Temps Cloud managed AI instead
              </span>
              <ArrowRight className="size-4" />
            </Link>
          </Button>
        ) : null}
      </div>
    </div>
  )
}

/** A concrete exchange, so "ask the assistant" is not an abstract promise. */
function AssistantExample({ roomy }: { roomy?: boolean }) {
  return (
    <div
      className={cn(
        'space-y-2 rounded-lg border border-border bg-muted/30 p-3 text-sm',
        roomy && 'space-y-3 p-5 text-base'
      )}
    >
      <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
        Example
      </p>
      <p
        className={cn(
          'rounded-md bg-background px-3 py-2',
          roomy && 'px-4 py-3'
        )}
      >
        {ASSISTANT_EXAMPLE_PROMPTS[0]}
      </p>
      <p
        className={cn(
          'px-1 text-xs leading-5 text-muted-foreground',
          roomy && 'text-sm leading-6'
        )}
      >
        Reads and changes run straight away unless you turn on “Ask before
        changes”. Deletions always wait for your approval.
      </p>
    </div>
  )
}

class AssistantStartError extends Error {
  constructor(
    readonly status: number | undefined,
    readonly problem: unknown,
    message: string
  ) {
    super(message)
  }
}

/**
 * Entry point for a new assistant chat: a prompt box and example questions.
 * The conversation is created when the first question is sent, so opening the
 * assistant and walking away leaves nothing behind.
 */
export function AssistantStart({
  onStarted,
  onNavigate,
  cloudPath,
  roomy,
}: {
  onStarted: (conversation: ConversationResponse, firstMessage: string) => void
  onNavigate?: () => void
  cloudPath?: string
  /** Full-screen sizing: larger type and controls, content centred. */
  roomy?: boolean
}) {
  const status = useQuery({
    ...getAiProviderStatusOptions(),
    staleTime: 15_000,
    retry: false,
  })
  const [blocked, setBlocked] = useState<{
    reason: string
    setupPath: string
  } | null>(null)
  const [draft, setDraft] = useState('')

  const start = useMutation({
    mutationFn: async (firstMessage: string) => {
      const { data, error, response } = await createAssistantConversation({
        body: {},
      })
      if (!data) {
        throw new AssistantStartError(
          response?.status,
          error,
          getErrorMessage(error, 'Could not start the assistant.')
        )
      }
      return { conversation: data, firstMessage }
    },
    onSuccess: ({ conversation, firstMessage }) =>
      onStarted(conversation, firstMessage),
    onError: (error) => {
      // 409 is "no provider configured", and carries where to fix it. Show the
      // onboarding in place rather than a toast the user cannot act on.
      if (error instanceof AssistantStartError && error.status === 409) {
        const readiness = assistantReadiness(status.data)
        setBlocked({
          reason:
            readiness.state === 'unconfigured'
              ? readiness.reason
              : 'No AI provider is configured.',
          setupPath:
            setupPathFromProblem(error.problem) ??
            (readiness.state === 'unconfigured'
              ? readiness.setupPath
              : '/ai-gateway?add=openrouter'),
        })
        return
      }
      toast.error('Could not start the assistant', {
        description: error.message,
      })
    },
  })

  if (status.isPending) return <AssistantStartSkeleton roomy={roomy} />

  const readiness = assistantReadiness(status.data)
  if (blocked || readiness.state === 'unconfigured') {
    const view =
      blocked ?? (readiness.state === 'unconfigured' ? readiness : null)
    return (
      <AssistantOnboarding
        reason={view?.reason ?? ''}
        setupPath={view?.setupPath ?? '/ai-gateway?add=openrouter'}
        cloudPath={cloudPath}
        roomy={roomy}
        onNavigate={onNavigate}
        onConfigured={() => setBlocked(null)}
      />
    )
  }

  const submit = (text: string) => {
    const message = text.trim()
    if (!message || start.isPending) return
    start.mutate(message)
  }

  return (
    <div
      className={cn(
        'flex h-full min-h-0 flex-col justify-end gap-4 p-5',
        roomy && 'justify-center gap-8 p-6'
      )}
    >
      <div className={cn('space-y-2', roomy && 'space-y-4')}>
        <h2
          className={cn(
            'flex items-center gap-2 text-xl font-semibold tracking-tight',
            roomy && 'gap-3 text-4xl'
          )}
        >
          <Sparkles
            className={cn('size-5 text-primary', roomy && 'size-8')}
            aria-hidden="true"
          />
          What should we look at?
        </h2>
        <p
          className={cn(
            'text-sm leading-6 text-muted-foreground',
            roomy && 'text-lg leading-8'
          )}
        >
          Ask about your projects, deployments or settings. Deletions always
          wait for your approval.
        </p>
      </div>
      <div className={cn('grid gap-2', roomy && 'gap-3')}>
        {ASSISTANT_EXAMPLE_PROMPTS.map((prompt) => (
          <button
            key={prompt}
            type="button"
            disabled={start.isPending}
            onClick={() => submit(prompt)}
            className={cn(
              'rounded-md border border-border px-3 py-2 text-left text-sm transition-colors hover:bg-accent disabled:opacity-50',
              roomy && 'rounded-lg px-5 py-4 text-base'
            )}
          >
            {prompt}
          </button>
        ))}
      </div>
      <form
        className="flex gap-2"
        onSubmit={(event) => {
          event.preventDefault()
          submit(draft)
        }}
      >
        <Input
          className={cn(roomy && 'h-12 text-base md:text-base')}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder="Ask Temps to inspect or change something…"
          aria-label="Message the Temps assistant"
          disabled={start.isPending}
          maxLength={8000}
        />
        <Button
          type="submit"
          size="icon"
          className={cn(roomy && 'size-12 shrink-0')}
          disabled={!draft.trim() || start.isPending}
          aria-label="Send message"
        >
          {start.isPending ? (
            <Loader2 className="size-4 animate-spin" aria-hidden="true" />
          ) : (
            <Send className="size-4" aria-hidden="true" />
          )}
        </Button>
      </form>
    </div>
  )
}

function AssistantStartSkeleton({ roomy }: { roomy?: boolean }) {
  return (
    <div
      className={cn('mt-auto space-y-3 p-5', roomy && 'my-auto space-y-5 p-6')}
      role="status"
      aria-label="Loading"
    >
      <Skeleton className="h-6 w-48" />
      <Skeleton className="h-4 w-full" />
      <Skeleton className="h-9 w-full" />
      <Skeleton className="h-9 w-full" />
      <Skeleton className="h-9 w-full" />
    </div>
  )
}

/**
 * An assistant conversation: the shared chat panel plus the one control the
 * assistant adds, Simple / Advanced.
 *
 * Simple is a chat box on the default model. Advanced adds the model and
 * reasoning pickers and opens every tool call so its arguments and result are
 * visible. Whether changes ask first is a switch inside the composer, so it is
 * reachable in both modes.
 */
export function AssistantThread({
  conversation,
  firstMessage,
  level,
  onLevelChange,
  readOnly,
}: {
  conversation: Pick<ConversationResponse, 'public_id' | 'context_id'>
  firstMessage?: string
  level: ChatDetailLevel
  onLevelChange: (level: ChatDetailLevel) => void
  readOnly?: boolean
}) {
  return (
    <div className="flex h-full min-h-0 flex-col gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Tabs
          value={level}
          onValueChange={(value) =>
            onLevelChange(value === 'advanced' ? 'advanced' : 'simple')
          }
        >
          <TabsList className="h-8">
            <TabsTrigger value="simple" className="px-3 py-1 text-xs">
              Simple
            </TabsTrigger>
            <TabsTrigger value="advanced" className="px-3 py-1 text-xs">
              Advanced
            </TabsTrigger>
          </TabsList>
        </Tabs>
      </div>
      <div className="min-h-0 flex-1">
        <DebugChatPanel
          key={conversation.public_id}
          conversationPublicId={conversation.public_id}
          userScoped
          contextType={ASSISTANT_CONTEXT_TYPE}
          contextId={conversation.context_id}
          detailLevel={level}
          initialMessage={firstMessage}
          emptyHint="Ask about your projects, deployments or settings."
          placeholder="Ask Temps to inspect or change something…"
          readOnly={readOnly}
        />
      </div>
    </div>
  )
}
