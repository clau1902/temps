// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  FolderTree,
  GitCompare,
  Layers,
  Monitor,
  Rocket,
  SquareTerminal,
  type LucideIcon,
} from 'lucide-react'

const TABS: ReadonlyArray<{ icon: LucideIcon; name: string; what: string }> = [
  {
    icon: Layers,
    name: 'Made here',
    what: 'projects, services and domains the chat created',
  },
  {
    icon: Monitor,
    name: 'Preview',
    what: 'the app running in the chat’s sandbox',
  },
  { icon: FolderTree, name: 'Files', what: 'every file the assistant writes' },
  {
    icon: GitCompare,
    name: 'Changes',
    what: 'a diff of each file it edited',
  },
  {
    icon: SquareTerminal,
    name: 'Terminal',
    what: 'a shell in the same sandbox',
  },
  {
    icon: Rocket,
    name: 'Deployments',
    what: 'build stages and the live link, as each one runs',
  },
]

/**
 * The work view's side panel before the chat exists. The tabs need a
 * conversation to show anything, so this says what each will hold and how to
 * get there, instead of leaving half the screen blank.
 */
export function WorkTabsOnboarding() {
  return (
    <aside
      aria-label="Working tabs"
      className="flex h-full min-h-0 min-w-0 flex-col items-center justify-center gap-4 overflow-y-auto bg-background p-6"
    >
      <div className="max-w-sm space-y-1 text-center">
        <p className="text-sm font-medium">The chat&rsquo;s work shows here</p>
        <p className="text-sm text-muted-foreground">
          Send a first message and these tabs fill in as the assistant works.
          Try &ldquo;Create a landing page for a barber shop&rdquo;.
        </p>
      </div>
      <ul className="w-full max-w-sm divide-y rounded-md border text-sm">
        {TABS.map(({ icon: Icon, name, what }) => (
          <li key={name} className="flex items-start gap-3 px-3 py-2">
            <Icon
              className="mt-0.5 size-4 shrink-0 text-muted-foreground"
              aria-hidden="true"
            />
            <span>
              <span className="font-medium">{name}</span>
              <span className="text-muted-foreground">: {what}</span>
            </span>
          </li>
        ))}
      </ul>
    </aside>
  )
}
