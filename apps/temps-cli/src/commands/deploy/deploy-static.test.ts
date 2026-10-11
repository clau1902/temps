// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { test, expect, describe } from 'bun:test'
import { formatFileSize, getContentType, isValidArchive } from './deploy-static.js'

describe('isValidArchive', () => {
  test('accepts .tar.gz as a two-part extension', () => {
    expect(isValidArchive('/tmp/site.tar.gz')).toBe(true)
  })

  test('accepts .tgz and .zip', () => {
    expect(isValidArchive('/tmp/site.tgz')).toBe(true)
    expect(isValidArchive('/tmp/site.zip')).toBe(true)
  })

  test('is case-insensitive', () => {
    expect(isValidArchive('/tmp/SITE.TAR.GZ')).toBe(true)
    expect(isValidArchive('/tmp/SITE.ZIP')).toBe(true)
  })

  test('rejects anything else, including bare .gz', () => {
    // A bare .gz (not .tar.gz) is not something the server can unpack — this
    // guards against silently uploading an unsupported bundle.
    expect(isValidArchive('/tmp/site.gz')).toBe(false)
    expect(isValidArchive('/tmp/site.rar')).toBe(false)
    expect(isValidArchive('/tmp/site')).toBe(false)
  })
})

describe('getContentType', () => {
  test('maps tar.gz/tgz to gzip', () => {
    expect(getContentType('/tmp/site.tar.gz')).toBe('application/gzip')
    expect(getContentType('/tmp/site.tgz')).toBe('application/gzip')
  })

  test('maps .zip to zip', () => {
    expect(getContentType('/tmp/site.zip')).toBe('application/zip')
  })

  test('falls back to octet-stream for anything unrecognized', () => {
    expect(getContentType('/tmp/site.rar')).toBe('application/octet-stream')
  })
})

describe('formatFileSize', () => {
  test('renders bytes under 1 KB as whole bytes', () => {
    expect(formatFileSize(512)).toBe('512 B')
  })

  test('renders KB with one decimal', () => {
    expect(formatFileSize(2048)).toBe('2.0 KB')
  })

  test('renders MB with two decimals', () => {
    expect(formatFileSize(1024 * 1024 * 3)).toBe('3.00 MB')
  })
})

describe('static bundles leave out what Temps never publishes', () => {
  async function project(): Promise<string> {
    const { mkdtemp, mkdir, writeFile } = await import('node:fs/promises')
    const { tmpdir } = await import('node:os')
    const { join } = await import('node:path')
    const root = await mkdtemp(join(tmpdir(), 'temps-static-'))
    for (const dir of ['.git', '.temps', '.claude/skills/x', 'assets', '.well-known', 'docs/.cache']) {
      await mkdir(join(root, dir), { recursive: true })
    }
    const files: Record<string, string> = {
      'index.html': '<h1>hi</h1>',
      'assets/app.js': 'console.log(1)',
      'assets/.DS_Store': 'mac',
      '.env': 'SECRET=1',
      '.git/config': '[core]',
      '.temps/config.json': '{}',
      '.claude/skills/x/SKILL.md': '# x',
      '.well-known/security.txt': 'Contact: x',
      'docs/.cache/a': 'a',
      'docs/guide.html': 'guide',
    }
    for (const [path, content] of Object.entries(files)) await writeFile(join(root, path), content)
    return root
  }

  test('keeps public files and a top-level .well-known, skips every other dot path', async () => {
    const { collectPublishableFiles } = await import('./deploy-static.js')
    const { rm } = await import('node:fs/promises')
    const root = await project()
    try {
      const { files, skipped } = await collectPublishableFiles(root)
      expect(files).toEqual(['.well-known/security.txt', 'assets/app.js', 'docs/guide.html', 'index.html'])
      expect(skipped).toEqual(['.claude', '.env', '.git', '.temps', 'assets/.DS_Store', 'docs/.cache'])
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })

  test('the archive contains exactly the publishable files', async () => {
    const { createTarGzFromDirectory } = await import('./deploy-static.js')
    const { mkdtemp, rm, writeFile } = await import('node:fs/promises')
    const { tmpdir } = await import('node:os')
    const { join } = await import('node:path')
    const { spawnSync } = await import('node:child_process')
    const root = await project()
    const out = await mkdtemp(join(tmpdir(), 'temps-static-out-'))
    try {
      const { data, skipped } = await createTarGzFromDirectory(root)
      expect(skipped).toContain('.temps')
      await writeFile(join(out, 'bundle.tar.gz'), data)
      const listing = spawnSync('tar', ['-tzf', join(out, 'bundle.tar.gz')], { encoding: 'utf-8' })
      const entries = listing.stdout.split('\n').filter(Boolean).sort()
      expect(entries).toEqual(['.well-known/security.txt', 'assets/app.js', 'docs/guide.html', 'index.html'])
    } finally {
      await rm(root, { recursive: true, force: true })
      await rm(out, { recursive: true, force: true })
    }
  })

  test('a folder with nothing publishable fails with what was left out', async () => {
    const { createTarGzFromDirectory } = await import('./deploy-static.js')
    const { mkdtemp, mkdir, rm } = await import('node:fs/promises')
    const { tmpdir } = await import('node:os')
    const { join } = await import('node:path')
    const root = await mkdtemp(join(tmpdir(), 'temps-static-empty-'))
    await mkdir(join(root, '.git'))
    try {
      await expect(createTarGzFromDirectory(root)).rejects.toThrow(/Nothing to publish.*\.git/)
    } finally {
      await rm(root, { recursive: true, force: true })
    }
  })
})
