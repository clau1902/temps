// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { getChatSandboxOptions } from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Skeleton } from '@/components/ui/skeleton'
import { useQuery } from '@tanstack/react-query'
import { ExternalLink, Monitor, RefreshCw } from 'lucide-react'
import { useEffect, useState, type FormEvent } from 'react'
import { canEmbed, parsePort, previewUrlFor } from './chat-preview'

function PreviewOnboarding({ reason }: { reason: string }) {
  return (
    <div className="flex flex-col items-center gap-2 p-6 text-center">
      <Monitor className="size-7 text-muted-foreground" aria-hidden />
      <p className="text-sm font-medium">No preview yet</p>
      <p className="max-w-xs text-sm text-muted-foreground">
        {reason} Try &ldquo;Create a landing page for a barber shop&rdquo;: the
        page opens here once it is running.
      </p>
    </div>
  )
}

/**
 * The page the chat's sandbox is serving, shown beside the conversation. The
 * port is the one the assistant last confirmed; any other can be typed in.
 */
export function ChatPreviewTab({
  conversationPublicId,
}: {
  conversationPublicId: string
}) {
  const status = useQuery(
    getChatSandboxOptions({ path: { public_id: conversationPublicId } })
  )
  const confirmed = status.data?.preview?.port ?? null
  const [port, setPort] = useState<number | null>(confirmed)
  const [draft, setDraft] = useState(confirmed ? String(confirmed) : '')
  const [reloads, setReloads] = useState(0)
  // The assistant starting a server on another port takes the tab with it.
  useEffect(() => {
    if (confirmed === null) return
    setPort(confirmed)
    setDraft(String(confirmed))
  }, [confirmed])

  if (status.isPending) {
    return (
      <div className="space-y-3 p-4" role="status" aria-label="Loading">
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-64 w-full" />
      </div>
    )
  }
  if (status.isError) {
    return (
      <p className="p-4 text-sm text-muted-foreground">
        Temps could not load this chat&rsquo;s sandbox. It will try again
        shortly.
      </p>
    )
  }
  const { configured, linked, preview, reason } = status.data
  if (!configured) {
    return (
      <PreviewOnboarding reason={reason ?? 'This chat cannot use a sandbox.'} />
    )
  }
  if (!linked || !preview) {
    return (
      <PreviewOnboarding reason="This chat has no sandbox yet. When the assistant builds something, it runs in one." />
    )
  }

  const url = port === null ? null : previewUrlFor(preview.url_template, port)
  const embeddable = url !== null && canEmbed(url, window.location.protocol)
  const submit = (event: FormEvent) => {
    event.preventDefault()
    const next = parsePort(draft)
    if (next !== null) setPort(next)
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <form
        onSubmit={submit}
        className="flex shrink-0 items-center gap-1.5 border-b px-3 py-2"
      >
        <label htmlFor="preview-port" className="text-xs text-muted-foreground">
          Port
        </label>
        <Input
          id="preview-port"
          inputMode="numeric"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder="3000"
          className="h-8 w-20"
          aria-invalid={draft !== '' && parsePort(draft) === null}
        />
        <Button
          type="submit"
          size="sm"
          variant="outline"
          className="h-8"
          disabled={parsePort(draft) === null}
        >
          Show
        </Button>
        <span className="flex-1" />
        {url && (
          <>
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="size-8"
              onClick={() => setReloads((count) => count + 1)}
              aria-label="Reload the preview"
            >
              <RefreshCw className="size-4" />
            </Button>
            <Button
              asChild
              size="icon"
              variant="ghost"
              className="size-8"
              aria-label="Open the preview in a new tab"
            >
              <a href={url} target="_blank" rel="noopener noreferrer">
                <ExternalLink className="size-4" />
              </a>
            </Button>
          </>
        )}
      </form>
      {url === null ? (
        <p className="p-4 text-sm text-muted-foreground">
          Enter the port your app listens on, or ask the assistant to run it and
          show it. It appears here as soon as the assistant confirms it answers.
        </p>
      ) : embeddable ? (
        <iframe
          key={`${url}:${reloads}`}
          src={url}
          title="Preview of the app running in this chat's sandbox"
          className="min-h-0 w-full flex-1 bg-white"
          sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals"
        />
      ) : (
        <p className="p-4 text-sm text-muted-foreground">
          This console is served over HTTPS and the preview is not, so the
          browser will not show it here. Open it in a new tab instead.
        </p>
      )}
    </div>
  )
}
