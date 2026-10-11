// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Side-effectful steps for `temps start` / `temps stop`: Docker, downloads,
 * the background server process and the state file. Node APIs only, because
 * the published CLI runs on Node.
 */

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { createReadStream, createWriteStream, existsSync, openSync, closeSync, readFileSync, statSync } from 'node:fs'
import { chmod, cp, mkdir, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:net'
import { dirname, join } from 'node:path'
import { Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'
import { fileURLToPath } from 'node:url'
import {
  DB_CONTAINER,
  DB_IMAGE,
  DB_NAME,
  DB_USER,
  DB_VOLUME,
  LOCAL_DOMAIN,
  RELEASE_REPO,
  StartError,
  binaryPath,
  databaseUrl,
  isLocalServerCommand,
  localUrls,
  parseChecksum,
  parseJsonOutput,
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

/** Run a command to completion. Secrets go in `env`, never in `args`. */
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
    child.stdout.on('data', (chunk: Buffer) => (stdout += chunk.toString()))
    child.stderr.on('data', (chunk: Buffer) => (stderr += chunk.toString()))
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
  try {
    return JSON.parse(await readFile(paths.state, 'utf-8')) as LocalState
  } catch (error) {
    throw new StartError(
      'state',
      'bad_state',
      `Could not read ${paths.state}: ${(error as Error).message}. Run \`temps stop --delete-data\` to start fresh.`,
    )
  }
}

/** The state file holds the database and admin passwords: owner-only. */
export async function writeState(paths: LocalPaths, state: LocalState): Promise<void> {
  await mkdir(paths.root, { recursive: true, mode: 0o700 })
  const tmp = `${paths.state}.tmp`
  await writeFile(tmp, JSON.stringify(state, null, 2) + '\n', { mode: 0o600 })
  await rename(tmp, paths.state)
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

function serverEnv(state: LocalState, paths: LocalPaths): NodeJS.ProcessEnv {
  return { TEMPS_DATABASE_URL: databaseUrl(state), TEMPS_DATA_DIR: paths.data }
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

function tailFile(path: string, count = 15): string {
  try {
    return lastLines(readFileSync(path, 'utf-8').replace(/\x1b\[[0-9;]*m/g, ''), count)
  } catch {
    return '(no log yet)'
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

export async function stopDatabase(): Promise<boolean> {
  if ((await containerState(DB_CONTAINER)) !== 'running') return false
  const result = await docker(['stop', DB_CONTAINER])
  if (result.code !== 0) {
    throw new StartError('database', 'stop_failed', `Could not stop ${DB_CONTAINER}: ${lastLines(result.stderr, 3)}`)
  }
  return true
}

/**
 * Remove the database container, its volume and everything in
 * `~/.temps/local` except the downloaded binaries, so starting fresh does
 * not download Temps again.
 */
export async function deleteLocalData(paths: LocalPaths): Promise<void> {
  await docker(['rm', '-f', DB_CONTAINER])
  const volume = await docker(['volume', 'rm', DB_VOLUME])
  if (volume.code !== 0 && !/no such volume/i.test(volume.stderr)) {
    throw new StartError('database', 'volume_rm_failed', `Could not remove volume ${DB_VOLUME}: ${lastLines(volume.stderr, 3)}`)
  }
  if (!existsSync(paths.root)) return
  for (const entry of await readdir(paths.root)) {
    if (join(paths.root, entry) === paths.bin) continue
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
