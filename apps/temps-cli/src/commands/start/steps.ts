// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Side-effectful steps for `temps start` / `temps stop`: Docker, downloads,
 * the background server process and the state file. Node APIs only, because
 * the published CLI runs on Node.
 */

import { spawn } from 'node:child_process'
import { createHash, randomBytes } from 'node:crypto'
import { createReadStream, createWriteStream, existsSync, fstatSync, openSync, closeSync, readSync, statSync } from 'node:fs'
import { chmod, cp, link, mkdir, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { basename, dirname, join } from 'node:path'
import { Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'
import { fileURLToPath } from 'node:url'
import {
  DB_CONTAINER,
  DB_IMAGE,
  DB_NAME,
  DB_USER,
  DB_VOLUME,
  DOCKER_NETWORK,
  LOCAL_DOMAIN,
  RELEASE_REPO,
  StartError,
  binaryPath,
  databaseUrl,
  isLocalServerCommand,
  isLocalState,
  localUrls,
  parseChecksum,
  parseJsonOutput,
  parseLocalServerPids,
  pickRelease,
  releaseAssetUrl,
  skillTargets,
  type Channel,
  type LocalPaths,
  type LocalState,
  type ReleaseSummary,
} from './runtime.js'

export interface RunResult {
  code: number | null
  stdout: string
  stderr: string
}

/** Most output kept from a child process: enough for any result, bounded for a runaway one. */
const MAX_CAPTURED_OUTPUT = 1024 * 1024

function appendCapped(text: string, chunk: Buffer): string {
  const next = text + chunk.toString()
  return next.length > MAX_CAPTURED_OUTPUT ? next.slice(-MAX_CAPTURED_OUTPUT) : next
}

/**
 * Run a command to completion. Secrets go in `env`, never in `args`. Keeps
 * at most the last `MAX_CAPTURED_OUTPUT` characters of each stream.
 */
export function run(
  command: string,
  args: readonly string[],
  options: { env?: NodeJS.ProcessEnv; timeoutMs?: number } = {},
): Promise<RunResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      env: { ...process.env, ...options.env },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk: Buffer) => (stdout = appendCapped(stdout, chunk)))
    child.stderr.on('data', (chunk: Buffer) => (stderr = appendCapped(stderr, chunk)))
    const timer = options.timeoutMs
      ? setTimeout(() => child.kill('SIGTERM'), options.timeoutMs)
      : undefined
    child.on('error', (error) => {
      if (timer) clearTimeout(timer)
      reject(error)
    })
    child.on('close', (code) => {
      if (timer) clearTimeout(timer)
      resolve({ code, stdout, stderr })
    })
  })
}

function lastLines(text: string, count = 8): string {
  return text.trim().split('\n').slice(-count).join('\n')
}

// ── State ────────────────────────────────────────────────────────────────

export async function readState(paths: LocalPaths): Promise<LocalState | null> {
  if (!existsSync(paths.state)) return null
  let parsed: unknown
  try {
    parsed = JSON.parse(await readFile(paths.state, 'utf-8'))
  } catch (error) {
    throw new StartError(
      'state',
      'bad_state',
      `Could not read ${paths.state}: ${(error as Error).message}. Run \`temps stop --delete-data\` to start fresh.`,
    )
  }
  if (!isLocalState(parsed)) {
    throw new StartError(
      'state',
      'bad_state',
      `${paths.state} is not a valid state file. Run \`temps stop --delete-data\` to start fresh.`,
    )
  }
  return parsed
}

/** The state file holds the database and admin passwords: owner-only. */
export async function writeState(paths: LocalPaths, state: LocalState): Promise<void> {
  await mkdir(paths.root, { recursive: true, mode: 0o700 })
  const tmp = `${paths.state}.tmp`
  await writeFile(tmp, JSON.stringify(state, null, 2) + '\n', { mode: 0o600 })
  await rename(tmp, paths.state)
}

// ── Lock ─────────────────────────────────────────────────────────────────

/** A lock this old is abandoned even if its pid is alive (reused by another program). */
const LOCK_STALE_MS = 60 * 60_000

interface LockToken {
  pid: number
  command: string
  startedAt: number
  nonce: string
}

function parseLock(raw: string): LockToken | null {
  try {
    const token = JSON.parse(raw) as Partial<LockToken>
    return typeof token.pid === 'number' && typeof token.startedAt === 'number' && typeof token.command === 'string'
      ? (token as LockToken)
      : null
  } catch {
    return null
  }
}

function lockBusy(paths: LocalPaths, holder: LockToken | null): StartError {
  const who = holder ? `\`temps ${holder.command}\` (pid ${holder.pid})` : 'Another temps start or stop'
  return new StartError(
    'lock',
    'busy',
    `${who} is already working on your local Temps. Wait for it to finish, then run this again. ` +
      `If nothing is running, delete ${paths.lock}.`,
  )
}

/**
 * Take the lock that serializes `temps start` and `temps stop`. Two of them
 * at once would each generate credentials, write `state.json` and unpack the
 * binary, and the loser's password would not match the database the winner
 * created. Fails at once when the lock is held: the caller should not queue
 * behind a command that may be waiting on a prompt.
 *
 * The lock is a hard link to a private token file, so taking it is atomic.
 * A lock whose process is gone (Ctrl-C, crash) or that is older than
 * `LOCK_STALE_MS` is taken over by renaming it aside first and checking that
 * what was renamed is the lock that was judged stale, so a live lock that
 * replaced it in between is put back instead of stolen. Returns the release
 * function.
 */
export async function acquireLock(paths: LocalPaths, command: string): Promise<() => Promise<void>> {
  await mkdir(paths.root, { recursive: true, mode: 0o700 })
  const nonce = randomBytes(8).toString('hex')
  const token = JSON.stringify({ pid: process.pid, command, startedAt: Date.now(), nonce } satisfies LockToken)
  const mine = `${paths.lock}.${process.pid}.${nonce}`
  await writeFile(mine, token, { mode: 0o600 })
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        await link(mine, paths.lock)
        return async () => {
          const held = await readFile(paths.lock, 'utf-8').catch(() => null)
          if (held === token) await rm(paths.lock, { force: true })
        }
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
      }
      const held = await readFile(paths.lock, 'utf-8').catch(() => null)
      if (held === null) continue // released between the link and the read
      const holder = parseLock(held)
      if (holder && processAlive(holder.pid) && Date.now() - holder.startedAt < LOCK_STALE_MS) {
        throw lockBusy(paths, holder)
      }
      const aside = `${mine}.stale`
      try {
        await rename(paths.lock, aside)
      } catch {
        continue // someone else took the stale lock away first
      }
      const taken = await readFile(aside, 'utf-8').catch(() => null)
      if (taken !== held) {
        await link(aside, paths.lock).catch(() => undefined)
        await rm(aside, { force: true })
        throw lockBusy(paths, taken ? parseLock(taken) : null)
      }
      await rm(aside, { force: true })
    }
    throw lockBusy(paths, null)
  } finally {
    await rm(mine, { force: true })
  }
}

export async function withLock<T>(paths: LocalPaths, command: string, work: () => Promise<T>): Promise<T> {
  const release = await acquireLock(paths, command)
  try {
    return await work()
  } finally {
    await release()
  }
}

// ── Preflight ────────────────────────────────────────────────────────────

export async function dockerVersion(): Promise<string> {
  let result: RunResult
  try {
    result = await run('docker', ['info', '--format', '{{.ServerVersion}}'], { timeoutMs: 15_000 })
  } catch {
    throw new StartError(
      'preflight',
      'docker_missing',
      'Docker is not installed. Install Docker Desktop, OrbStack or Colima, start it, and run `temps start` again.',
    )
  }
  if (result.code !== 0) {
    throw new StartError(
      'preflight',
      'docker_not_running',
      `Docker is installed but not running (${lastLines(result.stderr, 2) || 'no response'}). Start Docker Desktop, OrbStack or Colima, then run \`temps start\` again.`,
    )
  }
  return result.stdout.trim()
}

export function isPortFree(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const server = createServer()
    server.once('error', () => resolve(false))
    server.listen(port, '127.0.0.1', () => server.close(() => resolve(true)))
  })
}

export async function pickPort(
  candidates: readonly number[],
  taken: readonly number[],
  label: string,
): Promise<number> {
  for (const port of candidates) {
    if (taken.includes(port)) continue
    if (await isPortFree(port)) return port
  }
  throw new StartError(
    'preflight',
    'no_free_port',
    `No free port for the ${label} (tried ${candidates[0]}–${candidates[candidates.length - 1]} on 127.0.0.1).`,
  )
}

// ── Server binary ────────────────────────────────────────────────────────

async function fetchOk(url: string, timeoutMs: number): Promise<Response> {
  const headers: Record<string, string> = { 'User-Agent': 'temps-cli' }
  if (url.startsWith('https://api.github.com/') && process.env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`
  }
  let response: Response
  try {
    response = await fetch(url, { headers, signal: AbortSignal.timeout(timeoutMs), redirect: 'follow' })
  } catch (error) {
    throw new StartError('download', 'network', `Could not reach ${url}: ${(error as Error).message}`)
  }
  if (!response.ok) {
    throw new StartError('download', 'http_error', `GET ${url} returned HTTP ${response.status}.`)
  }
  return response
}

/** Newest release on `channel` that has an archive for this machine. */
export async function resolveVersion(channel: Channel, target: string): Promise<string> {
  const releases: ReleaseSummary[] = []
  if (channel === 'stable') {
    const latest = (await (await fetchOk(`https://api.github.com/repos/${RELEASE_REPO}/releases/latest`, 15_000)).json()) as ReleaseSummary
    releases.push(latest)
  } else {
    for (let page = 1; page <= 5; page++) {
      const batch = (await (
        await fetchOk(`https://api.github.com/repos/${RELEASE_REPO}/releases?per_page=100&page=${page}`, 15_000)
      ).json()) as ReleaseSummary[]
      releases.push(...batch)
      if (batch.length < 100 || pickRelease(releases, channel, target)) break
    }
  }
  const version = pickRelease(releases, channel, target)
  if (!version) {
    throw new StartError(
      'download',
      'no_release',
      `No ${channel} release of Temps has a ${target} build. Try --channel nightly, or pin one with --version.`,
    )
  }
  return version
}

async function sha256File(path: string): Promise<string> {
  const hash = createHash('sha256')
  await pipeline(createReadStream(path), hash)
  return hash.digest('hex')
}

/**
 * Download, verify and unpack the server binary for `version`. A version
 * already on disk is reused, so later starts need no network.
 */
export async function ensureBinary(
  paths: LocalPaths,
  version: string,
  target: string,
): Promise<{ path: string; downloaded: boolean; bytes: number }> {
  const bin = binaryPath(paths, version)
  if (existsSync(bin)) return { path: bin, downloaded: false, bytes: 0 }

  const dir = dirname(bin)
  const staging = `${dir}.partial`
  await rm(staging, { recursive: true, force: true })
  await mkdir(staging, { recursive: true, mode: 0o700 })
  const archive = join(staging, `${target}.tar.gz`)
  try {
    const expected = parseChecksum(
      await (await fetchOk(releaseAssetUrl(version, target, '.tar.gz.sha256'), 30_000)).text(),
    )
    const response = await fetchOk(releaseAssetUrl(version, target), 10 * 60_000)
    if (!response.body) {
      throw new StartError('download', 'empty_body', `The ${target} archive for ${version} came back empty.`)
    }
    await pipeline(Readable.fromWeb(response.body as never), createWriteStream(archive, { mode: 0o600 }))
    const actual = await sha256File(archive)
    if (actual !== expected) {
      throw new StartError(
        'download',
        'checksum_mismatch',
        `The ${target} archive for ${version} failed its checksum (expected ${expected}, got ${actual}). Nothing was installed.`,
      )
    }
    const untar = await run('tar', ['-xzf', archive, '-C', staging], { timeoutMs: 120_000 })
    if (untar.code !== 0) {
      throw new StartError('download', 'extract_failed', `Could not unpack ${archive}: ${lastLines(untar.stderr, 3)}`)
    }
    await rm(archive, { force: true })
    if (!existsSync(join(staging, 'temps'))) {
      throw new StartError('download', 'no_binary', `The ${target} archive for ${version} has no \`temps\` binary.`)
    }
    await chmod(join(staging, 'temps'), 0o755)
    await rm(dir, { recursive: true, force: true })
    await rename(staging, dir)
    return { path: bin, downloaded: true, bytes: statSync(bin).size }
  } catch (error) {
    await rm(staging, { recursive: true, force: true })
    throw error
  }
}

// ── Database ─────────────────────────────────────────────────────────────

async function docker(args: readonly string[], options: { env?: NodeJS.ProcessEnv; timeoutMs?: number } = {}): Promise<RunResult> {
  return run('docker', args, { timeoutMs: 60_000, ...options })
}

export async function containerState(name: string): Promise<'missing' | 'running' | 'stopped'> {
  const result = await docker(['inspect', '--format', '{{.State.Running}}', name], { timeoutMs: 15_000 })
  if (result.code !== 0) return 'missing'
  return result.stdout.trim() === 'true' ? 'running' : 'stopped'
}

/**
 * Start the local database, creating it on first run. The password reaches
 * `docker run` through the environment (`-e NAME` with no value), so it
 * never appears in a process listing.
 */
export async function ensureDatabase(state: LocalState): Promise<{ created: boolean; pulled: boolean }> {
  const current = await containerState(DB_CONTAINER)
  let created = false
  let pulled = false
  if (current === 'stopped') {
    const started = await docker(['start', DB_CONTAINER])
    if (started.code !== 0) {
      throw new StartError('database', 'start_failed', `Could not start ${DB_CONTAINER}: ${lastLines(started.stderr, 3)}`)
    }
  } else if (current === 'missing') {
    const image = await docker(['image', 'inspect', DB_IMAGE], { timeoutMs: 15_000 })
    if (image.code !== 0) {
      const pull = await docker(['pull', '--quiet', DB_IMAGE], { timeoutMs: 20 * 60_000 })
      if (pull.code !== 0) {
        throw new StartError('database', 'pull_failed', `Could not pull ${DB_IMAGE}: ${lastLines(pull.stderr, 3)}`)
      }
      pulled = true
    }
    const result = await docker(
      [
        'run', '-d',
        '--name', DB_CONTAINER,
        '--label', 'sh.temps.local=database',
        '--restart', 'unless-stopped',
        '-e', `POSTGRES_USER=${DB_USER}`,
        '-e', `POSTGRES_DB=${DB_NAME}`,
        '-e', 'POSTGRES_PASSWORD',
        '-v', `${DB_VOLUME}:/var/lib/postgresql`,
        '-p', `127.0.0.1:${state.dbPort}:5432`,
        DB_IMAGE,
      ],
      { env: { POSTGRES_PASSWORD: state.dbPassword } },
    )
    if (result.code !== 0) {
      throw new StartError('database', 'run_failed', `Could not create ${DB_CONTAINER}: ${lastLines(result.stderr, 3)}`)
    }
    created = true
  }

  const deadline = Date.now() + 90_000
  while (Date.now() < deadline) {
    const ready = await docker(['exec', DB_CONTAINER, 'pg_isready', '-U', DB_USER, '-h', '127.0.0.1'], { timeoutMs: 10_000 })
    if (ready.code === 0) return { created, pulled }
    await new Promise((resolve) => setTimeout(resolve, 250))
  }
  const logs = await docker(['logs', '--tail', '20', DB_CONTAINER])
  throw new StartError(
    'database',
    'not_ready',
    `${DB_CONTAINER} did not accept connections within 90s. Last log lines:\n${lastLines(logs.stdout + logs.stderr)}`,
  )
}

// ── Platform setup and API key ───────────────────────────────────────────

/**
 * Environment for every invocation of the server binary. The network name
 * scopes this install's containers (see `DOCKER_NETWORK`).
 */
function serverEnv(state: LocalState, paths: LocalPaths): NodeJS.ProcessEnv {
  return {
    TEMPS_DATABASE_URL: databaseUrl(state),
    TEMPS_DATA_DIR: paths.data,
    TEMPS_NETWORK_NAME: DOCKER_NETWORK,
  }
}

/**
 * First run only: create the admin and point the platform at the local
 * wildcard domain. `temps setup --auto` also runs the database migrations.
 */
export async function runSetup(bin: string, state: LocalState, paths: LocalPaths): Promise<void> {
  await mkdir(paths.data, { recursive: true, mode: 0o700 })
  const result = await run(
    bin,
    [
      'setup',
      '--auto',
      '--admin-email', state.adminEmail,
      '--wildcard-domain', `*.${LOCAL_DOMAIN}`,
      '--server-ip', '127.0.0.1',
      '--external-url', localUrls(state.proxyPort).external,
      '--output-format', 'json',
    ],
    {
      env: { ...serverEnv(state, paths), TEMPS_ADMIN_PASSWORD: state.adminPassword },
      timeoutMs: 5 * 60_000,
    },
  )
  if (result.code !== 0) {
    throw new StartError(
      'setup',
      'setup_failed',
      `\`temps setup\` exited with ${result.code}:\n${lastLines(result.stderr || result.stdout)}`,
    )
  }
  const output = parseJsonOutput<{ success?: boolean; message?: string }>(result.stdout, 'setup')
  if (output.success !== true) {
    throw new StartError('setup', 'setup_failed', `\`temps setup\` reported failure: ${output.message ?? 'no message'}`)
  }
}

export interface MintedKey {
  apiKey: string
  keyPrefix: string
}

/**
 * Mint an admin API key for this machine's CLI and agents. Its value travels
 * only through this process's pipes and lands in the CLI's 0600 context file.
 */
export async function mintApiKey(bin: string, state: LocalState, paths: LocalPaths, name: string): Promise<MintedKey> {
  const result = await run(
    bin,
    // No --email: older releases lack it, and the local admin is the only user.
    ['api-key', '--name', name, '--role', 'admin', '--output-format', 'json'],
    { env: serverEnv(state, paths), timeoutMs: 60_000 },
  )
  if (result.code !== 0) {
    throw new StartError('api_key', 'mint_failed', `\`temps api-key\` exited with ${result.code}:\n${lastLines(result.stderr || result.stdout)}`)
  }
  const output = parseJsonOutput<{ api_key?: string; key_prefix?: string }>(result.stdout, 'api-key')
  if (!output.api_key) {
    throw new StartError('api_key', 'mint_failed', '`temps api-key` returned no key.')
  }
  return { apiKey: output.api_key, keyPrefix: output.key_prefix ?? output.api_key.slice(0, 8) }
}

// ── Server process ───────────────────────────────────────────────────────

export function processAlive(pid: number | undefined): boolean {
  if (!pid) return false
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/** Start `temps serve` in the background, logging to `server.log`. */
export async function startServer(bin: string, state: LocalState, paths: LocalPaths): Promise<number> {
  await mkdir(paths.root, { recursive: true, mode: 0o700 })
  const log = openSync(paths.log, 'a', 0o600)
  try {
    const child = spawn(
      bin,
      ['serve', '--address', `127.0.0.1:${state.proxyPort}`, '--console-address', `127.0.0.1:${state.consolePort}`],
      {
        detached: true,
        stdio: ['ignore', log, log],
        env: { ...process.env, ...serverEnv(state, paths) },
      },
    )
    if (!child.pid) {
      throw new StartError('server', 'spawn_failed', `Could not start ${bin} serve.`)
    }
    child.unref()
    return child.pid
  } finally {
    closeSync(log)
  }
}

/** How much of the end of a log is read to show its last lines. */
const TAIL_BYTES = 64 * 1024

/**
 * The last `count` lines of a log, reading at most `TAIL_BYTES` from its
 * end: `server.log` grows across restarts and must never be loaded whole
 * just to report an error.
 */
export function tailFile(path: string, count = 15): string {
  let fd: number | undefined
  try {
    fd = openSync(path, 'r')
    const size = fstatSync(fd).size
    const length = Math.min(size, TAIL_BYTES)
    const buffer = Buffer.alloc(length)
    const read = readSync(fd, buffer, 0, length, size - length)
    let text = buffer.subarray(0, read).toString('utf-8')
    // A window that starts mid-file starts mid-line: drop the partial line.
    if (size > length) text = text.slice(text.indexOf('\n') + 1)
    return lastLines(text.replace(/\x1b\[[0-9;]*m/g, ''), count) || '(log is empty)'
  } catch {
    return '(no log yet)'
  } finally {
    if (fd !== undefined) closeSync(fd)
  }
}

export async function serverHealthy(consolePort: number): Promise<boolean> {
  try {
    const response = await fetch(`http://127.0.0.1:${consolePort}/health`, { signal: AbortSignal.timeout(1_000) })
    return response.ok
  } catch {
    return false
  }
}

export async function waitForServer(state: LocalState, paths: LocalPaths, pid: number, timeoutMs = 120_000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (await serverHealthy(state.consolePort)) return
    if (!processAlive(pid)) {
      throw new StartError(
        'server',
        'exited',
        `Temps exited during startup. Last lines of ${paths.log}:\n${tailFile(paths.log)}`,
      )
    }
    await new Promise((resolve) => setTimeout(resolve, 200))
  }
  throw new StartError(
    'server',
    'not_ready',
    `Temps did not become healthy within ${timeoutMs / 1000}s. Last lines of ${paths.log}:\n${tailFile(paths.log)}`,
  )
}

/** Whether `pid` is alive and is the server `temps start` launched. */
export async function isLocalServer(pid: number | undefined, paths: LocalPaths): Promise<boolean> {
  if (!processAlive(pid)) return false
  try {
    const ps = await run('ps', ['-o', 'command=', '-p', String(pid)], { timeoutMs: 5_000 })
    return ps.code === 0 && isLocalServerCommand(ps.stdout, paths)
  } catch {
    return false
  }
}

/**
 * SIGTERM, then SIGKILL after 15s. Returns false if it was not running, or
 * if the saved pid now belongs to another program, which is left alone.
 */
export async function stopServer(pid: number | undefined, paths: LocalPaths): Promise<boolean> {
  if (!(await isLocalServer(pid, paths))) return false
  process.kill(pid as number, 'SIGTERM')
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    if (!processAlive(pid)) return true
    await new Promise((resolve) => setTimeout(resolve, 200))
  }
  process.kill(pid as number, 'SIGKILL')
  return true
}

/**
 * Pids of every running local server, from the process table. `null` when
 * `ps` cannot list processes here (the caller falls back to the saved pid).
 */
export async function localServerPids(paths: LocalPaths): Promise<number[] | null> {
  try {
    const ps = await run('ps', ['-axo', 'pid=,command='], { timeoutMs: 10_000 })
    return ps.code === 0 ? parseLocalServerPids(ps.stdout, paths) : null
  } catch {
    return null
  }
}

/**
 * Stop every local server process. Works from the process table, so it does
 * not need a readable state file; `savedPid` is only a fallback where `ps`
 * is unavailable, and is verified before it is signalled either way.
 */
export async function stopServers(paths: LocalPaths, savedPid: number | undefined): Promise<boolean> {
  const pids = (await localServerPids(paths)) ?? (savedPid === undefined ? [] : [savedPid])
  let stopped = false
  for (const pid of pids) stopped = (await stopServer(pid, paths)) || stopped
  return stopped
}

export async function stopDatabase(): Promise<boolean> {
  if ((await containerState(DB_CONTAINER)) !== 'running') return false
  const result = await docker(['stop', DB_CONTAINER])
  if (result.code !== 0) {
    throw new StartError('database', 'stop_failed', `Could not stop ${DB_CONTAINER}: ${lastLines(result.stderr, 3)}`)
  }
  return true
}

export interface RemovedWorkloads {
  containers: string[]
  volumes: string[]
  /** Volumes or the network Docker refused to remove, with its reason. */
  kept: string[]
}

/**
 * Remove what this install deployed: every container on `DOCKER_NETWORK`
 * (apps, Compose stacks, managed services), the volumes Temps created for
 * them, and the network. Once the platform database is gone nothing else
 * can manage them, so they must go with it.
 *
 * Scoped by network membership, which no other Temps on this Docker engine
 * shares. A volume those containers mounted is removed only if it was
 * created after this install's database volume, so a volume that already
 * existed and was merely attached to a deployed app is kept. Docker itself
 * refuses to remove a volume another container still uses. Everything kept
 * is reported with its reason, never forced.
 */
export async function removeWorkloads(
  network: string = DOCKER_NETWORK,
  installVolume: string = DB_VOLUME,
): Promise<RemovedWorkloads> {
  const removed: RemovedWorkloads = { containers: [], volumes: [], kept: [] }
  const listed = await docker(['ps', '-aq', '--no-trunc', '--filter', `network=${network}`])
  if (listed.code !== 0) {
    throw new StartError('workloads', 'list_failed', `Could not list containers on ${network}: ${lastLines(listed.stderr, 3)}`)
  }
  const ids = listed.stdout.split('\n').map((line) => line.trim()).filter(Boolean)
  const volumes = new Set<string>()
  if (ids.length > 0) {
    const format = '{{.Name}}\t{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}} {{end}}{{end}}'
    const inspected = await docker(['inspect', '--format', format, ...ids])
    for (const line of inspected.stdout.split('\n')) {
      const [name, mounts = ''] = line.split('\t')
      if (!name) continue
      removed.containers.push(name.replace(/^\//, ''))
      for (const volume of mounts.split(' ').filter(Boolean)) volumes.add(volume)
    }
    // -v also removes the containers' anonymous volumes.
    const rmContainers = await docker(['rm', '-f', '-v', ...ids], { timeoutMs: 5 * 60_000 })
    if (rmContainers.code !== 0) {
      throw new StartError('workloads', 'rm_failed', `Could not remove this install's containers: ${lastLines(rmContainers.stderr, 3)}`)
    }
  }
  const installedAt = await volumeCreatedAt(installVolume)
  for (const volume of [...volumes].sort()) {
    const createdAt = await volumeCreatedAt(volume)
    if (createdAt === null) continue // an anonymous volume `rm -v` already removed
    if (installedAt === null || createdAt < installedAt) {
      removed.kept.push(
        installedAt === null
          ? `volume ${volume}: cannot tell whether this install created it (${installVolume} is missing)`
          : `volume ${volume}: it existed before this install`,
      )
      continue
    }
    const rmVolume = await docker(['volume', 'rm', volume])
    if (rmVolume.code === 0) removed.volumes.push(volume)
    else if (!/no such volume/i.test(rmVolume.stderr)) removed.kept.push(`volume ${volume}: ${lastLines(rmVolume.stderr, 1)}`)
  }
  const rmNetwork = await docker(['network', 'rm', network])
  if (rmNetwork.code !== 0 && !/not found|no such network/i.test(rmNetwork.stderr)) {
    removed.kept.push(`network ${network}: ${lastLines(rmNetwork.stderr, 1)}`)
  }
  return removed
}

/** When a volume was created (ms since the epoch), or null if it does not exist. */
async function volumeCreatedAt(name: string): Promise<number | null> {
  const inspected = await docker(['volume', 'inspect', '--format', '{{.CreatedAt}}', name], { timeoutMs: 15_000 })
  const createdAt = inspected.code === 0 ? Date.parse(inspected.stdout.trim()) : Number.NaN
  return Number.isNaN(createdAt) ? null : createdAt
}

/**
 * Destroy this install's sandboxes through its own API, the only place
 * that knows which sandbox containers are its own (they are not on
 * `DOCKER_NETWORK`). Returns the ids destroyed and the ones the server
 * refused, with its reason.
 */
export async function destroySandboxes(apiUrl: string, apiKey: string): Promise<{ destroyed: string[]; failed: string[] }> {
  const headers = { Authorization: `Bearer ${apiKey}` }
  const destroyed: string[] = []
  const failed: string[] = []
  // Each sandbox is tried once. A destroyed sandbox leaves the listing, so
  // after a page that destroyed something the same page is read again; a
  // page that destroyed nothing (all refused) is stepped past. The round
  // limit bounds both the requests and the ids remembered.
  const attempted = new Set<string>()
  let page = 1
  for (let round = 0; round < 200; round++) {
    const response = await fetch(`${apiUrl}/v1/sandboxes?page=${page}&page_size=100`, { headers, signal: AbortSignal.timeout(15_000) })
    if (!response.ok) {
      // Older releases have no sandbox API: nothing to destroy.
      if (response.status === 404) break
      throw new StartError('workloads', 'sandbox_list_failed', `GET ${apiUrl}/v1/sandboxes returned HTTP ${response.status}.`)
    }
    const listed = ((await response.json()) as { sandboxes?: { id: string }[] }).sandboxes ?? []
    if (listed.length === 0) break
    let destroyedHere = 0
    for (const sandbox of listed) {
      if (attempted.has(sandbox.id)) continue
      attempted.add(sandbox.id)
      const result = await fetch(`${apiUrl}/v1/sandboxes/${encodeURIComponent(sandbox.id)}/destroy`, {
        method: 'POST',
        headers,
        signal: AbortSignal.timeout(120_000),
      })
      if (result.ok || result.status === 404) {
        destroyed.push(sandbox.id)
        destroyedHere += 1
      } else {
        failed.push(`${sandbox.id}: HTTP ${result.status}`)
      }
    }
    if (destroyedHere === 0) page += 1
  }
  return { destroyed, failed }
}

/** Names of every sandbox container on this Docker engine, whichever Temps made it. */
export async function sandboxContainers(): Promise<string[]> {
  const listed = await docker(['ps', '-a', '--filter', 'label=sh.temps.sandbox=true', '--format', '{{.Names}}'])
  return listed.code === 0 ? listed.stdout.split('\n').map((line) => line.trim()).filter(Boolean) : []
}

/**
 * Remove the database container, its volume and everything in
 * `~/.temps/local` except the downloaded binaries (so starting fresh does
 * not download Temps again) and the lock the caller holds.
 */
export async function deleteLocalData(paths: LocalPaths): Promise<void> {
  await docker(['rm', '-f', DB_CONTAINER])
  const volume = await docker(['volume', 'rm', DB_VOLUME])
  if (volume.code !== 0 && !/no such volume/i.test(volume.stderr)) {
    throw new StartError('database', 'volume_rm_failed', `Could not remove volume ${DB_VOLUME}: ${lastLines(volume.stderr, 3)}`)
  }
  if (!existsSync(paths.root)) return
  for (const entry of await readdir(paths.root)) {
    if (join(paths.root, entry) === paths.bin || entry.startsWith(basename(paths.lock))) continue
    await rm(join(paths.root, entry), { recursive: true, force: true })
  }
}

// ── Skills ───────────────────────────────────────────────────────────────

/**
 * The Temps skills ship inside the npm package (`dist/skills`); from a
 * source checkout they are the repo's `skills/` directory.
 */
export function findBundledSkills(from: string = fileURLToPath(import.meta.url)): string | null {
  let dir = dirname(from)
  for (let i = 0; i < 7; i++) {
    const candidate = join(dir, 'skills')
    if (existsSync(join(candidate, 'temps-cli', 'SKILL.md'))) return candidate
    const parent = dirname(dir)
    if (parent === dir) break
    dir = parent
  }
  return null
}

/** Copy every bundled skill into each agent's skills directory under `base`. */
export async function installSkills(source: string, base: string): Promise<{ skills: string[]; targets: string[] }> {
  const entries = await readdir(source, { withFileTypes: true })
  const skills = entries
    .filter((entry) => entry.isDirectory() && existsSync(join(source, entry.name, 'SKILL.md')))
    .map((entry) => entry.name)
    .sort()
  const targets = skillTargets(base)
  for (const target of targets) {
    await mkdir(target, { recursive: true })
    for (const skill of skills) {
      await rm(join(target, skill), { recursive: true, force: true })
      await cp(join(source, skill), join(target, skill), { recursive: true })
    }
  }
  return { skills, targets }
}
