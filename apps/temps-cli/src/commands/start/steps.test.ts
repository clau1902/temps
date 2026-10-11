// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { afterEach, describe, expect, test } from 'bun:test'
import { existsSync, mkdtempSync, readFileSync, statSync } from 'node:fs'
import { mkdir, readdir, rm, utimes, writeFile } from 'node:fs/promises'
import { spawnSync } from 'node:child_process'
import { createServer as createHttpServer } from 'node:http'
import { createServer, type AddressInfo } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { localPaths, type LocalState } from './runtime.js'
import {
  acquireLock,
  destroySandboxes,
  findBundledSkills,
  installSkills,
  isLocalServer,
  isPortFree,
  pickPort,
  processAlive,
  readState,
  removeWorkloads,
  stopServer,
  tailFile,
  withLock,
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

describe('state file validation', () => {
  test('rejects unparseable and wrongly shaped state with the reset hint', async () => {
    const paths = localPaths(tempDir())
    await mkdir(paths.root, { recursive: true })
    await writeFile(paths.state, '{ not json')
    await expect(readState(paths)).rejects.toThrow(/Could not read .*state\.json.*temps stop --delete-data/)
    await writeFile(paths.state, JSON.stringify({ version: 'v1', pid: 'abc' }))
    await expect(readState(paths)).rejects.toThrow(/not a valid state file.*temps stop --delete-data/)
  })
})

describe('start/stop lock', () => {
  test('a second command fails while the first holds the lock, and succeeds after release', async () => {
    const paths = localPaths(tempDir())
    const release = await acquireLock(paths, 'start')
    await expect(acquireLock(paths, 'stop')).rejects.toThrow(new RegExp(`temps start.*pid ${process.pid}.*already working`))
    await release()
    expect(existsSync(paths.lock)).toBe(false)
    await (await acquireLock(paths, 'stop'))()
    // No token or stale files are left behind.
    expect(await readdir(paths.root)).toEqual([])
  })

  test('overlapping callers: exactly one runs at a time', async () => {
    const paths = localPaths(tempDir())
    let inside = 0
    let ran = 0
    const attempt = () =>
      withLock(paths, 'start', async () => {
        inside += 1
        expect(inside).toBe(1)
        await new Promise((resolve) => setTimeout(resolve, 30))
        inside -= 1
        ran += 1
      })
    const results = await Promise.allSettled([attempt(), attempt(), attempt(), attempt()])
    expect(ran).toBe(1)
    expect(results.filter((r) => r.status === 'rejected')).toHaveLength(3)
    expect(existsSync(paths.lock)).toBe(false)
  })

  test('releases the lock when the work throws', async () => {
    const paths = localPaths(tempDir())
    await expect(withLock(paths, 'start', async () => Promise.reject(new Error('boom')))).rejects.toThrow('boom')
    expect(existsSync(paths.lock)).toBe(false)
  })

  test('takes over a lock whose process is gone', async () => {
    const paths = localPaths(tempDir())
    await mkdir(paths.root, { recursive: true })
    await writeFile(paths.lock, JSON.stringify({ pid: 2 ** 22 + 12345, command: 'start', startedAt: Date.now(), nonce: 'x' }))
    const release = await acquireLock(paths, 'stop')
    expect(JSON.parse(readFileSync(paths.lock, 'utf-8')).pid).toBe(process.pid)
    await release()
  })

  test('takes over an abandoned lock even if its pid was reused, and an unreadable one', async () => {
    const paths = localPaths(tempDir())
    await mkdir(paths.root, { recursive: true })
    const twoHoursAgo = Date.now() - 2 * 60 * 60_000
    await writeFile(paths.lock, JSON.stringify({ pid: process.pid, command: 'start', startedAt: twoHoursAgo, nonce: 'x' }))
    await (await acquireLock(paths, 'stop'))()
    await writeFile(paths.lock, 'garbage')
    await (await acquireLock(paths, 'stop'))()
    expect(existsSync(paths.lock)).toBe(false)
  })

  test('release leaves a lock that is no longer ours', async () => {
    const paths = localPaths(tempDir())
    const release = await acquireLock(paths, 'start')
    const other = JSON.stringify({ pid: process.pid, command: 'stop', startedAt: Date.now(), nonce: 'other' })
    await writeFile(paths.lock, other)
    await release()
    expect(readFileSync(paths.lock, 'utf-8')).toBe(other)
  })
})

describe('tailFile', () => {
  test('returns the last lines of a small log, without colour codes', async () => {
    const dir = tempDir()
    const log = join(dir, 'server.log')
    await writeFile(log, 'one\n\x1b[31mtwo\x1b[0m\nthree\n')
    expect(tailFile(log, 2)).toBe('two\nthree')
    expect(tailFile(join(dir, 'missing.log'))).toBe('(no log yet)')
  })

  test('reads only the end of a large log and drops the partial first line', async () => {
    const dir = tempDir()
    const log = join(dir, 'server.log')
    // ~6 MB of numbered lines; the tail must not depend on the size.
    const line = (n: number) => `line ${String(n).padStart(7, '0')} ${'x'.repeat(40)}\n`
    let body = ''
    for (let n = 0; n < 110_000; n++) body += line(n)
    await writeFile(log, body)
    await utimes(log, new Date(), new Date())
    const tail = tailFile(log, 3).split('\n')
    expect(tail).toEqual([109_997, 109_998, 109_999].map((n) => line(n).trimEnd()))
    // Every returned line is whole: none starts mid-line.
    for (const entry of tailFile(log, 2000).split('\n')) expect(entry).toMatch(/^line \d{7} x{40}$/)
    expect(tailFile(log, 2000).split('\n').length).toBeLessThan(1400)
  })
})

describe('destroySandboxes', () => {
  /** A stand-in for the sandbox API: lists what is left, destroys on request. */
  async function fakeApi(ids: string[], refuse: Record<string, number> = {}, listStatus = 200) {
    const alive = new Set(ids)
    const seen: string[] = []
    const server = createHttpServer((req, res) => {
      seen.push(`${req.method} ${req.url} ${req.headers.authorization}`)
      const destroy = /^\/api\/v1\/sandboxes\/([^/]+)\/destroy$/.exec(req.url ?? '')
      if (req.method === 'POST' && destroy) {
        const id = decodeURIComponent(destroy[1] as string)
        if (refuse[id]) return void res.writeHead(refuse[id] as number).end()
        alive.delete(id)
        return void res.writeHead(204).end()
      }
      if (listStatus !== 200) return void res.writeHead(listStatus).end()
      const query = new URL(req.url ?? '', 'http://localhost').searchParams
      const page = Number(query.get('page') ?? '1')
      const size = Number(query.get('page_size') ?? '20')
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ sandboxes: [...alive].slice((page - 1) * size, page * size).map((id) => ({ id })) }))
    })
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
    const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/api`
    return { url, alive, seen, close: () => new Promise((resolve) => server.close(resolve)) }
  }

  test('destroys every sandbox, across pages, with the API key', async () => {
    const ids = Array.from({ length: 230 }, (_, n) => `sbx_${n}`)
    const api = await fakeApi(ids)
    try {
      const result = await destroySandboxes(api.url, 'tk_test')
      expect(result.destroyed).toHaveLength(230)
      expect(result.failed).toEqual([])
      expect(api.alive.size).toBe(0)
      expect(api.seen.every((line) => line.endsWith('Bearer tk_test'))).toBe(true)
    } finally {
      await api.close()
    }
  })

  test('reports a sandbox the server refuses to destroy, once, and moves on', async () => {
    const api = await fakeApi(['a', 'busy', 'c'], { busy: 409 })
    try {
      const result = await destroySandboxes(api.url, 'tk_test')
      expect(result.destroyed.sort()).toEqual(['a', 'c'])
      expect(result.failed).toEqual(['busy: HTTP 409'])
      expect(api.seen.filter((line) => line.startsWith('POST /api/v1/sandboxes/busy/destroy'))).toHaveLength(1)
    } finally {
      await api.close()
    }
  })

  test('a full page of refused sandboxes does not hide the ones after it', async () => {
    const busy = Array.from({ length: 100 }, (_, n) => `busy_${n}`)
    const api = await fakeApi([...busy, 'x', 'y'], Object.fromEntries(busy.map((id) => [id, 409])))
    try {
      const result = await destroySandboxes(api.url, 'tk_test')
      expect(result.destroyed.sort()).toEqual(['x', 'y'])
      expect(result.failed).toHaveLength(100)
      // Every sandbox was tried exactly once.
      expect(api.seen.filter((line) => line.startsWith('POST '))).toHaveLength(102)
    } finally {
      await api.close()
    }
  })

  test('a server without the sandbox API has nothing to destroy; other failures are errors', async () => {
    const none = await fakeApi([], {}, 404)
    const broken = await fakeApi([], {}, 500)
    try {
      expect(await destroySandboxes(none.url, 'tk_test')).toEqual({ destroyed: [], failed: [] })
      await expect(destroySandboxes(broken.url, 'tk_test')).rejects.toThrow(/v1\/sandboxes returned HTTP 500/)
    } finally {
      await none.close()
      await broken.close()
    }
  })
})

describe('removeWorkloads', () => {
  const docker = (...args: string[]) => spawnSync('docker', args, { encoding: 'utf-8', timeout: 120_000 })
  const IMAGE = 'busybox:stable'

  test('removes only the containers on the install network and the volumes created for them', async () => {
    if (docker('info', '--format', '{{.ServerVersion}}').status !== 0) {
      console.log('Docker not available, skipping')
      return
    }
    if (docker('image', 'inspect', IMAGE).status !== 0 && docker('pull', '--quiet', IMAGE).status !== 0) {
      console.log(`Could not pull ${IMAGE}, skipping`)
      return
    }
    const id = `temps-cli-test-${process.pid}-${Date.now()}`
    const names = {
      network: `${id}-net`,
      otherNetwork: `${id}-other-net`,
      older: `${id}-older`,
      install: `${id}-install`,
      data: `${id}-data`,
      shared: `${id}-shared`,
      app: `${id}-app`,
      service: `${id}-service`,
      bystander: `${id}-bystander`,
    }
    const cleanup = () => {
      docker('rm', '-f', '-v', names.app, names.service, names.bystander)
      docker('volume', 'rm', names.older, names.install, names.data, names.shared)
      docker('network', 'rm', names.network, names.otherNetwork)
    }
    try {
      // A volume from before the install, then (a second later, the
      // resolution of Docker's CreatedAt) the install's own marker volume.
      expect(docker('volume', 'create', names.older).status).toBe(0)
      await new Promise((resolve) => setTimeout(resolve, 1100))
      for (const volume of [names.install, names.data, names.shared]) expect(docker('volume', 'create', volume).status).toBe(0)
      for (const network of [names.network, names.otherNetwork]) expect(docker('network', 'create', network).status).toBe(0)
      const create = (name: string, network: string, ...volumes: string[]) =>
        docker('create', '--name', name, '--network', network, ...volumes.flatMap((v) => ['-v', `${v}:/mnt/${v}`]), '-v', '/anonymous', IMAGE, 'true')
      expect(create(names.app, names.network, names.data, names.older).status).toBe(0)
      expect(create(names.service, names.network, names.shared).status).toBe(0)
      // Another Temps' container: same engine, different network, sharing one volume.
      expect(create(names.bystander, names.otherNetwork, names.shared).status).toBe(0)

      const removed = await removeWorkloads(names.network, names.install)

      expect(removed.containers.sort()).toEqual([names.app, names.service])
      expect(removed.volumes).toEqual([names.data])
      expect(removed.kept).toHaveLength(2)
      expect(removed.kept.join('\n')).toMatch(new RegExp(`volume ${names.older}: it existed before this install`))
      expect(removed.kept.join('\n')).toMatch(new RegExp(`volume ${names.shared}: .*in use`))
      const left = docker('ps', '-a', '--filter', `name=${id}`, '--format', '{{.Names}}').stdout.trim()
      expect(left).toBe(names.bystander)
      const volumesLeft = docker('volume', 'ls', '--filter', `name=${id}`, '--format', '{{.Name}}').stdout.trim().split('\n').sort()
      expect(volumesLeft).toEqual([names.install, names.older, names.shared])
      expect(docker('network', 'inspect', names.network).status).not.toBe(0)
      expect(docker('network', 'inspect', names.otherNetwork).status).toBe(0)

      // Nothing on the network: a no-op that reports nothing.
      expect(await removeWorkloads(names.network, names.install)).toEqual({ containers: [], volumes: [], kept: [] })
    } finally {
      cleanup()
    }
  }, 180_000)
})
