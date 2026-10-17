// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ImageSupport } from './assistant-models'
import type { ChatAttachment } from './chat-message-parts'

export interface AttachmentLimits {
  maxFiles: number
  maxBytes: number
}

/** Workspace chats keep files on disk. */
export const WORKSPACE_ATTACHMENT_LIMITS: AttachmentLimits = {
  maxFiles: 8,
  maxBytes: 20 * 1024 * 1024,
}

/**
 * Assistant chats keep files in the database and re-send images to the model on
 * every step of a turn, so they are held to less. Mirrors the server's caps.
 */
export const ASSISTANT_ATTACHMENT_LIMITS: AttachmentLimits = {
  maxFiles: 4,
  maxBytes: 5 * 1024 * 1024,
}

function megabytes(bytes: number): string {
  return `${Math.round(bytes / (1024 * 1024))} MB`
}

/** Reject the entire selection rather than silently dropping excess files. */
export function attachmentSelectionError(
  files: readonly Pick<File, 'name' | 'size'>[],
  pendingCount: number,
  limits: AttachmentLimits = WORKSPACE_ATTACHMENT_LIMITS
): string | null {
  if (files.length + pendingCount > limits.maxFiles)
    return `A message may include at most ${limits.maxFiles} files.`
  const oversized = files.find((file) => file.size > limits.maxBytes)
  return oversized
    ? `${oversized.name} exceeds the ${megabytes(limits.maxBytes)} attachment limit.`
    : null
}

/**
 * The assistant reads images only on models that accept them. Say so before
 * uploading, naming what to do, instead of letting the send fail later.
 */
export function assistantImageError(
  files: readonly Pick<File, 'name' | 'type'>[],
  support: ImageSupport
): string | null {
  if (support !== 'no') return null
  const image = files.find((file) => file.type.startsWith('image/'))
  return image
    ? `${image.name} is an image, and this model can only read text. Pick a model that accepts images, or attach a text file.`
    : null
}

/** What the file picker offers for an assistant chat on this model. */
export function assistantAttachmentAccept(support: ImageSupport): string {
  const text = 'text/*,.md,.log,.json,.yaml,.yml,.toml,.csv,.env,.conf'
  return support === 'no'
    ? text
    : `image/png,image/jpeg,image/gif,image/webp,${text}`
}

/** Helper line under the composer. Names what this model can be given. */
export function attachHint(
  isAssistantChat: boolean,
  support: ImageSupport,
  limits: AttachmentLimits
): string {
  if (!isAssistantChat) {
    return `Attach, drop, or paste images and files · up to ${limits.maxFiles} files, ${megabytes(limits.maxBytes)} each`
  }
  const what =
    support === 'no'
      ? 'text files (this model can\u2019t read images)'
      : 'images and text files'
  return `Attach, drop, or paste ${what} · up to ${limits.maxFiles} files, ${megabytes(limits.maxBytes)} each`
}

/** Tooltip for the attach button in an assistant chat. */
export function assistantAttachTitle(
  support: ImageSupport,
  limits: AttachmentLimits,
  hasConversation: boolean
): string {
  if (!hasConversation) return 'Send a first message before attaching files'
  const size = megabytes(limits.maxBytes)
  if (support === 'no')
    return `Attach text files (${size} each). This model can\u2019t read images.`
  if (support === 'unknown')
    return `Attach images or text files (${size} each). Images only work if the model can read them.`
  return `Attach images or text files (${size} each)`
}

interface ObjectUrlApi {
  createObjectURL(blob: Blob): string
  revokeObjectURL(url: string): void
}

export function revokeAttachmentPreviews(
  attachments: Pick<ChatAttachment, 'preview_url'>[],
  objectUrls: Pick<ObjectUrlApi, 'revokeObjectURL'> = URL
): void {
  for (const attachment of attachments) {
    if (attachment.preview_url) {
      objectUrls.revokeObjectURL(attachment.preview_url)
    }
  }
}

/**
 * Materialize an uploaded attachment only while its owning composer exists.
 * The second check closes the narrow unmount race after creating a blob URL.
 */
export function createPendingAttachment(
  payload: Omit<ChatAttachment, 'preview_url'>,
  file: File,
  isMounted: () => boolean,
  objectUrls: ObjectUrlApi = URL
): ChatAttachment | null {
  if (!isMounted()) return null
  const previewUrl = payload.is_image
    ? objectUrls.createObjectURL(file)
    : undefined
  if (!isMounted()) {
    if (previewUrl) objectUrls.revokeObjectURL(previewUrl)
    return null
  }
  return { ...payload, preview_url: previewUrl }
}
