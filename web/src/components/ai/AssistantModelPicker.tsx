// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { useIsMobile } from '@/components/hooks/use-mobile'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover'
import { Skeleton } from '@/components/ui/skeleton'
import { cn } from '@/lib/utils'
import { getErrorMessage } from '@/utils/errorHandling'
import { Check, ChevronsUpDown, Loader2, TriangleAlert } from 'lucide-react'
import { useMemo, useState } from 'react'
import {
  useAssistantProviderKey,
  useEffectiveDefaultModel,
  useProviderModels,
  useSetDefaultModel,
} from './assistant-model-hooks'
import { toast } from 'sonner'
import {
  describeModel,
  filterAssistantModels,
  lacksToolCalling,
  modelDisplayName,
  type AssistantModelOption,
} from './assistant-models'
import { ModelVendorLogo } from './ModelVendorLogo'
import {
  filterByVendorTab,
  modelVendor,
  OTHER_VENDORS_KEY,
  vendorTabs,
  type VendorTab,
} from './model-vendors'

/**
 * Searchable list of the key's models. Each row shows what goes in and out and
 * what it costs, so the choice does not need a trip to the provider's website.
 */
export function AssistantModelList({
  providerKeyId,
  value,
  defaultModelId,
  onChange,
  disabled,
  roomy,
}: {
  providerKeyId: number | null
  value: string | null
  /** Marked "Default" in the list. */
  defaultModelId?: string | null
  onChange: (modelId: string) => void
  disabled?: boolean
  roomy?: boolean
}) {
  const models = useProviderModels(providerKeyId)
  const [query, setQuery] = useState('')
  const [tabKey, setTabKey] = useState<string | null>(null)
  const tabs = useMemo(() => vendorTabs(models.options), [models.options])
  // A search looks across every vendor; the tabs only narrow the browsing.
  const visible = useMemo(
    () =>
      filterAssistantModels(
        query.trim()
          ? models.options
          : filterByVendorTab(models.options, tabKey, tabs),
        query
      ),
    [models.options, query, tabKey, tabs]
  )

  if (models.isPending && providerKeyId !== null) {
    return (
      <div className="space-y-2" role="status" aria-label="Loading models">
        <Skeleton className="h-9 w-full" />
        <Skeleton className="h-14 w-full" />
        <Skeleton className="h-14 w-full" />
        <Skeleton className="h-14 w-full" />
      </div>
    )
  }
  if (models.isError || models.options.length === 0) {
    return (
      <p className="text-sm text-muted-foreground" role="status">
        {models.isError
          ? getErrorMessage(
              models.error,
              'The model list could not be loaded. The provider default will be used.'
            )
          : 'This provider has not listed any models yet. The provider default will be used.'}
      </p>
    )
  }

  return (
    <div className="space-y-2">
      <Input
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        placeholder={`Search ${models.options.length} models…`}
        aria-label="Search models"
        className={cn(roomy && 'h-11 text-base md:text-base')}
        disabled={disabled}
      />
      <ModelVendorTabs
        tabs={tabs}
        total={models.options.length}
        selected={query.trim() ? null : tabKey}
        searching={query.trim().length > 0}
        onSelect={setTabKey}
      />
      <ul
        className={cn(
          'max-h-[min(18rem,calc(var(--radix-popover-content-available-height,100vh)-5rem))] space-y-1 overflow-y-auto pr-1',
          roomy && 'max-h-80'
        )}
        aria-label="Models"
      >
        {visible.length === 0 ? (
          <li className="px-2 py-3 text-sm text-muted-foreground">
            No model matches “{query.trim()}”.
          </li>
        ) : (
          visible.map((option) => (
            <ModelRow
              key={option.id}
              option={option}
              selected={option.id === value}
              isDefault={option.id === defaultModelId}
              disabled={disabled}
              onSelect={() => onChange(option.id)}
            />
          ))
        )}
      </ul>
    </div>
  )
}

/**
 * Vendor tabs above the list, so the model of a given maker is a click away
 * instead of a scroll through several hundred. Hidden behind the search: a query
 * already spans every vendor.
 */
export function ModelVendorTabs({
  tabs,
  total,
  selected,
  searching,
  onSelect,
}: {
  tabs: readonly VendorTab[]
  total: number
  selected: string | null
  searching: boolean
  onSelect: (key: string | null) => void
}) {
  if (tabs.length < 2) return null
  return (
    <div
      role="tablist"
      aria-label="Filter models by vendor"
      className={cn('flex flex-wrap gap-1', searching && 'opacity-50')}
    >
      <VendorTabButton
        label="All"
        count={total}
        active={selected === null}
        onClick={() => onSelect(null)}
      />
      {tabs.map((tab) => (
        <VendorTabButton
          key={tab.key}
          label={tab.name}
          count={tab.count}
          logo={tab.key === OTHER_VENDORS_KEY ? undefined : tab}
          active={selected === tab.key}
          onClick={() => onSelect(tab.key)}
        />
      ))}
    </div>
  )
}

function VendorTabButton({
  label,
  count,
  logo,
  active,
  onClick,
}: {
  label: string
  count: number
  logo?: Pick<VendorTab, 'name' | 'logo'>
  active: boolean
  onClick: () => void
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className={cn(
        'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs transition-colors hover:bg-accent',
        active
          ? 'border-border bg-muted font-medium text-foreground'
          : 'border-transparent text-muted-foreground'
      )}
    >
      {logo ? <ModelVendorLogo vendor={logo} size={14} /> : null}
      {label}
      <span className="tabular-nums opacity-60">{count}</span>
    </button>
  )
}

function ModelRow({
  option,
  selected,
  isDefault,
  disabled,
  onSelect,
}: {
  option: AssistantModelOption
  selected: boolean
  isDefault: boolean
  disabled?: boolean
  onSelect: () => void
}) {
  const facts = describeModel(option)
  const vendor = modelVendor(option.id)
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        disabled={disabled}
        aria-pressed={selected}
        className={cn(
          'flex w-full items-start gap-2.5 rounded-md border border-transparent px-3 py-2 text-left transition-colors hover:bg-accent disabled:opacity-50',
          selected && 'border-border bg-muted'
        )}
      >
        <ModelVendorLogo vendor={vendor} size={20} className="mt-0.5" />
        <span className="min-w-0 flex-1">
          <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="truncate text-sm font-medium">{option.name}</span>
            {isDefault ? (
              <Badge
                variant="secondary"
                className="px-1.5 py-0 text-[11px] font-normal"
              >
                Default
              </Badge>
            ) : null}
            {lacksToolCalling(option) ? (
              <Badge
                variant="outline"
                className="gap-1 border-amber-500/50 px-1.5 py-0 text-[11px] font-normal text-amber-700 dark:text-amber-400"
              >
                <TriangleAlert className="size-3" aria-hidden="true" />
                No tool calling
              </Badge>
            ) : null}
          </span>
          {option.name !== option.id ? (
            <span className="block truncate font-mono text-xs text-muted-foreground">
              {option.id}
            </span>
          ) : null}
          {facts.length > 0 ? (
            <span className="mt-0.5 block text-xs text-muted-foreground">
              {facts.join(' · ')}
            </span>
          ) : null}
        </span>
        {selected ? (
          <Check className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
        ) : null}
      </button>
    </li>
  )
}

/**
 * Onboarding's second step, right after the key is saved: pick the model the
 * assistant starts on. Skipping keeps the provider's own default.
 */
export function AssistantModelSetup({
  providerKeyId,
  onDone,
  roomy,
}: {
  providerKeyId: number
  onDone: () => void
  roomy?: boolean
}) {
  const [choice, setChoice] = useState<string | null>(null)
  const setDefault = useSetDefaultModel(providerKeyId)
  const defaultModel = useEffectiveDefaultModel({ id: providerKeyId })

  const save = () => {
    if (choice === null) {
      onDone()
      return
    }
    setDefault.mutate(choice, {
      onSuccess: () => {
        toast.success('Default model saved')
        onDone()
      },
    })
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <h3 className={cn('text-base font-semibold', roomy && 'text-xl')}>
          Choose the default model
        </h3>
        <p
          className={cn(
            'text-sm leading-6 text-muted-foreground',
            roomy && 'text-base leading-7'
          )}
        >
          New chats start on this model. The assistant calls the Temps API as it
          works, so pick one that supports tool calling. You can change it at
          any time, per chat or as the default.
        </p>
      </div>
      <AssistantModelList
        providerKeyId={providerKeyId}
        value={choice ?? defaultModel}
        defaultModelId={defaultModel}
        onChange={setChoice}
        disabled={setDefault.isPending}
        roomy={roomy}
      />
      {setDefault.isError ? (
        <p className="text-sm text-destructive" role="alert">
          {getErrorMessage(setDefault.error, 'Could not save the model.')}
        </p>
      ) : null}
      <div className="flex flex-wrap justify-end gap-2">
        <Button
          type="button"
          variant="ghost"
          onClick={onDone}
          disabled={setDefault.isPending}
          className={cn(roomy && 'h-11 text-base')}
        >
          Keep {defaultModel ?? 'the suggested model'}
        </Button>
        <Button
          type="button"
          onClick={save}
          disabled={choice === null || setDefault.isPending}
          className={cn(roomy && 'h-11 px-6 text-base')}
        >
          {setDefault.isPending ? (
            <Loader2 className="size-4 animate-spin" aria-hidden="true" />
          ) : null}
          Use this model
        </Button>
      </div>
    </div>
  )
}

/**
 * The model of one chat, as a button that opens the list. It is controlled by
 * the chat that owns it: choosing here never changes the key's default, so two
 * chats can run on different models.
 */
export function AssistantModelChip({
  value,
  onChange,
  disabled,
  roomy,
}: {
  value: string | null
  onChange: (modelId: string) => void
  disabled?: boolean
  roomy?: boolean
}) {
  const { key } = useAssistantProviderKey()
  const providerKeyId = key?.id ?? null
  const models = useProviderModels(providerKeyId)
  const defaultModel = useEffectiveDefaultModel(key)
  const [open, setOpen] = useState(false)
  const isMobile = useIsMobile()

  const label = modelDisplayName(models.options, value) ?? 'Loading…'

  const choose = (modelId: string) => {
    setOpen(false)
    if (modelId !== value) onChange(modelId)
  }

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={disabled}
          className={cn(
            'h-8 min-w-0 max-w-44 justify-between gap-2 px-2 font-normal md:max-w-full text-muted-foreground hover:text-foreground',
            roomy && 'h-9 text-sm'
          )}
          aria-label={`Model: ${label}. Change model for this chat`}
          title="The model for this chat"
        >
          <span className="flex min-w-0 items-center gap-2">
            {value ? (
              <ModelVendorLogo vendor={modelVendor(value)} size={18} />
            ) : null}
            <span className="truncate font-medium text-foreground">
              {label}
            </span>
          </span>
          <ChevronsUpDown
            className="size-4 shrink-0 opacity-50"
            aria-hidden="true"
          />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="w-[min(32rem,calc(100vw-1rem))] p-3"
        onOpenAutoFocus={(event) => {
          // The keyboard would cover the list; search is one tap away.
          if (isMobile) event.preventDefault()
        }}
      >
        <AssistantModelList
          providerKeyId={providerKeyId}
          value={value}
          defaultModelId={defaultModel}
          onChange={choose}
          roomy={false}
        />
      </PopoverContent>
    </Popover>
  )
}
