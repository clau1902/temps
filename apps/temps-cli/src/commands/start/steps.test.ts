// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, describe, expect, test } from 'bun:test'
import { existsSync, mkdtempSync, readFileSync, statSync } from 'node:fs'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { localPaths, type LocalState } from './runtime.js'
import {
  findBundledSkills,
  installSkills,
  isLocalServer,
  isPortFree,
  pickPort,
  processAlive,
  readState,
  stopServer,
  writeState,
} from './steps.js'

const dirs: string[] = []
function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'temps-start-'))
  dirs.push(dir)
  return dir
}

afterEach(async () => {
  await Promise.all(dirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })))
})

const STATE: LocalState = {
  version: 'v0.1.0-beta.56',
  dbPort: 55432,
  dbPassword: 'db-secret',
  proxyPort: 8080,
  consolePort: 8700,
  adminEmail: 'admin@localhost',
  adminPassword: 'Tl-secret-9a',
  setupComplete: true,
}

describe('state file', () => {
  test('round-trips and is readable only by its owner', async () => {
    const paths = localPaths(tempDir())
    expect(await readState(paths)).toBeNull()
    await writeState(paths, STATE)
    expect(await readState(paths)).toEqual(STATE)
    expect(statSync(paths.state).mode & 0o777).toBe(0o600)
  })

  test('a corrupt file explains how to start fresh', async () => {
    const paths = localPaths(tempDir())
    await mkdir(paths.root, { recursive: true })
    await writeFile(paths.state, '{not json')
    await expect(readState(paths)).rejects.toThrow(/temps stop --delete-data/)
  })
})

describe('skills', () => {
  test('the repo skills are found from the source tree', () => {
    const found = findBundledSkills()
    expect(found).not.toBeNull()
    expect(existsSync(join(found as string, 'temps-cli', 'SKILL.md'))).toBe(true)
  })

  test('copies every skill into each agent folder and replaces old copies', async () => {
    const source = tempDir()
    await mkdir(join(source, 'deploy-to-temps', 'references'), { recursive: true })
    await writeFile(join(source, 'deploy-to-temps', 'SKILL.md'), '# new')
    await writeFile(join(source, 'deploy-to-temps', 'references', 'a.md'), 'ref')
    await mkdir(join(source, 'not-a-skill'))

    const base = tempDir()
    const stale = join(base, '.claude', 'skills', 'deploy-to-temps')
    await mkdir(stale, { recursive: true })
    await writeFile(join(stale, 'old.md'), 'stale')

    const result = await installSkills(source, base)
    expect(result.skills).toEqual(['deploy-to-temps'])
    for (const target of result.targets) {
      expect(readFileSync(join(target, 'deploy-to-temps', 'SKILL.md'), 'utf-8')).toBe('# new')
      expect(existsSync(join(target, 'deploy-to-temps', 'references', 'a.md'))).toBe(true)
      expect(existsSync(join(target, 'not-a-skill'))).toBe(false)
    }
    expect(existsSync(join(stale, 'old.md'))).toBe(false)
  })
})

describe('ports and processes', () => {
  test('skips a port another program holds', async () => {
    const server = createServer()
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
    const busy = (server.address() as { port: number }).port
    try {
      expect(await isPortFree(busy)).toBe(false)
      const picked = await pickPort([busy, busy + 1, busy + 2], [busy + 1], 'test')
      expect(picked).toBe(busy + 2)
    } finally {
      server.close()
    }
  })

  test('fails with the range it tried when nothing is free', async () => {
    await expect(pickPort([1, 2], [1, 2], 'console API')).rejects.toThrow(/console API.*1–2/)
  })

  test('knows whether a pid is alive', () => {
    expect(processAlive(process.pid)).toBe(true)
    expect(processAlive(undefined)).toBe(false)
    expect(processAlive(2 ** 22 + 12345)).toBe(false)
  })

  test('never signals a live pid that is not the local server', async () => {
    // This test runner is alive but is not `~/.temps/local/bin/<v>/temps serve`.
    const paths = localPaths(tempDir())
    expect(await isLocalServer(process.pid, paths)).toBe(false)
    expect(await stopServer(process.pid, paths)).toBe(false)
    expect(await stopServer(undefined, paths)).toBe(false)
    expect(processAlive(process.pid)).toBe(true)
  })
})
