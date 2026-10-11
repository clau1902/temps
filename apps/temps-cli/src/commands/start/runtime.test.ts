// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import {
  StartError,
  consoleApiUrl,
  databaseUrl,
  generateAdminPassword,
  isLocalServerCommand,
  localPaths,
  localUrls,
  parseChannel,
  parseChecksum,
  parseJsonOutput,
  pickRelease,
  proxyPortCandidates,
  releaseAssetUrl,
  releaseTarget,
  resolveSkillsScope,
  skillTargets,
} from './runtime.js'

describe('releaseTarget', () => {
  test('maps macOS and Linux on both architectures', () => {
    expect(releaseTarget('darwin', 'arm64')).toBe('temps-darwin-arm64')
    expect(releaseTarget('darwin', 'x64')).toBe('temps-darwin-amd64')
    expect(releaseTarget('linux', 'x64')).toBe('temps-linux-amd64')
    expect(releaseTarget('linux', 'arm64')).toBe('temps-linux-arm64')
  })

  test('rejects Windows with a pointer to the server installer', () => {
    expect(() => releaseTarget('win32', 'x64')).toThrow(/win32\/x64.*deploy\.sh/)
  })
})

describe('pickRelease', () => {
  const asset = (name: string) => [{ name: `${name}.tar.gz` }]
  const releases = [
    { tag_name: 'v0.1.0-nightly.20261010.abc', prerelease: true, assets: asset('temps-darwin-arm64') },
    { tag_name: 'v0.1.0-beta.57', prerelease: true, assets: asset('temps-linux-amd64') },
    { tag_name: 'v0.1.0-beta.56', prerelease: true, assets: asset('temps-darwin-arm64') },
    { tag_name: 'v0.0.9', draft: true, assets: asset('temps-darwin-arm64') },
    { tag_name: 'v0.0.8', prerelease: false, assets: asset('temps-darwin-arm64') },
  ]

  test('beta is the newest non-nightly with a build for this machine', () => {
    expect(pickRelease(releases, 'beta', 'temps-darwin-arm64')).toBe('v0.1.0-beta.56')
    expect(pickRelease(releases, 'beta', 'temps-linux-amd64')).toBe('v0.1.0-beta.57')
  })

  test('nightly only picks nightly builds', () => {
    expect(pickRelease(releases, 'nightly', 'temps-darwin-arm64')).toBe('v0.1.0-nightly.20261010.abc')
    expect(pickRelease(releases, 'nightly', 'temps-linux-amd64')).toBeNull()
  })

  test('stable skips prereleases and drafts', () => {
    expect(pickRelease(releases, 'stable', 'temps-darwin-arm64')).toBe('v0.0.8')
  })
})

describe('parseChecksum', () => {
  const digest = 'a'.repeat(64)

  test('reads the digest from a sha256sum line', () => {
    expect(parseChecksum(`${digest}  temps-darwin-arm64.tar.gz\n`)).toBe(digest)
    expect(parseChecksum(digest.toUpperCase())).toBe(digest)
  })

  test('rejects anything that is not a SHA-256 digest', () => {
    expect(() => parseChecksum('<html>Not Found</html>')).toThrow(StartError)
  })
})

describe('parseJsonOutput', () => {
  test('skips a log line printed before the JSON', () => {
    const stdout = '2026-10-11T05:01:17Z INFO Using Docker context colima\n{\n  "id": 1,\n  "api_key": "tk_x"\n}\n'
    expect(parseJsonOutput<{ api_key: string }>(stdout, 'api-key').api_key).toBe('tk_x')
  })

  test('names the command when there is no JSON', () => {
    expect(() => parseJsonOutput('done', 'setup')).toThrow(/temps setup/)
  })
})

describe('generateAdminPassword', () => {
  test('always has upper, lower, digit and symbol', () => {
    for (let i = 0; i < 50; i++) {
      const password = generateAdminPassword()
      expect(password).toMatch(/[A-Z]/)
      expect(password).toMatch(/[a-z]/)
      expect(password).toMatch(/[0-9]/)
      expect(password).toMatch(/[^A-Za-z0-9]/)
      expect(password.length).toBeGreaterThanOrEqual(24)
    }
  })

  test('uses the random source it is given', () => {
    const zeros = (n: number) => Buffer.alloc(n)
    expect(generateAdminPassword(zeros)).toBe(`Tl-${'A'.repeat(20)}-9a`)
  })
})

describe('URLs', () => {
  test('omits :80 and keeps other ports', () => {
    expect(localUrls(80).console).toBe('http://console.127-0-0-1.sslip.io')
    expect(localUrls(8080).app('my-app')).toBe('http://my-app.127-0-0-1.sslip.io:8080')
  })

  test('the CLI talks to the console listener directly', () => {
    expect(consoleApiUrl(8700)).toBe('http://127.0.0.1:8700/api')
  })

  test('the database password is URL-encoded', () => {
    expect(databaseUrl({ dbPort: 55432, dbPassword: 'a/b@c' })).toBe(
      'postgresql://temps:a%2Fb%40c@127.0.0.1:55432/temps?sslmode=disable',
    )
  })

  test('release assets come from the GitHub release', () => {
    expect(releaseAssetUrl('v0.1.0-beta.56', 'temps-darwin-arm64', '.tar.gz.sha256')).toBe(
      'https://github.com/gotempsh/temps/releases/download/v0.1.0-beta.56/temps-darwin-arm64.tar.gz.sha256',
    )
  })
})

describe('proxyPortCandidates', () => {
  test('tries the requested port first, then 8080 upwards', () => {
    expect(proxyPortCandidates(3001, 'darwin', false).slice(0, 3)).toEqual([3001, 8080, 8081])
  })

  test('only tries port 80 for root on Linux', () => {
    expect(proxyPortCandidates(undefined, 'linux', true)[0]).toBe(80)
    expect(proxyPortCandidates(undefined, 'linux', false)).not.toContain(80)
    expect(proxyPortCandidates(undefined, 'darwin', true)).not.toContain(80)
  })
})

describe('resolveSkillsScope', () => {
  test('installs into a project when the folder looks like one', () => {
    expect(resolveSkillsScope(undefined, ['src', 'package.json'])).toBe('project')
    expect(resolveSkillsScope(undefined, ['.git'])).toBe('project')
  })

  test('installs for the user anywhere else', () => {
    expect(resolveSkillsScope(undefined, ['Downloads', 'Desktop'])).toBe('global')
  })

  test('honours an explicit scope and rejects unknown ones', () => {
    expect(resolveSkillsScope('none', ['package.json'])).toBe('none')
    expect(() => resolveSkillsScope('everywhere', [])).toThrow(/project, global or none/)
  })

  test('targets the Claude Code and shared agent skill folders', () => {
    expect(skillTargets('/repo')).toEqual(['/repo/.claude/skills', '/repo/.agents/skills'])
  })
})

describe('parseChannel and paths', () => {
  test('defaults to beta and rejects unknown channels', () => {
    expect(parseChannel(undefined)).toBe('beta')
    expect(() => parseChannel('edge')).toThrow(/stable, beta, nightly/)
  })

  test('keeps everything under ~/.temps/local', () => {
    const paths = localPaths('/home/me')
    expect(paths.state).toBe('/home/me/.temps/local/state.json')
    expect(paths.log).toBe('/home/me/.temps/local/server.log')
  })
})

describe('isLocalServerCommand', () => {
  const paths = localPaths('/home/me')
  const serve = 'serve --address 127.0.0.1:8080 --console-address 127.0.0.1:8700'

  test('accepts the server temps start launched', () => {
    expect(isLocalServerCommand(`/home/me/.temps/local/bin/v0.1.0-beta.56/temps ${serve}\n`, paths)).toBe(true)
    expect(isLocalServerCommand('/home/me/.temps/local/bin/v1.2.3/temps serve', paths)).toBe(true)
  })

  test('accepts a home folder with spaces', () => {
    const spaced = localPaths('/Users/Ada Lovelace')
    expect(isLocalServerCommand(`/Users/Ada Lovelace/.temps/local/bin/v1.2.3/temps ${serve}`, spaced)).toBe(true)
  })

  test('rejects a reused pid: another program, another temps, another subcommand', () => {
    expect(isLocalServerCommand('/usr/bin/vim notes.txt', paths)).toBe(false)
    expect(isLocalServerCommand(`/usr/local/bin/temps ${serve}`, paths)).toBe(false)
    expect(isLocalServerCommand('/home/me/.temps/local/bin/v1.2.3/temps setup --auto', paths)).toBe(false)
    expect(isLocalServerCommand('/home/me/.temps/local/bin/v1.2.3/other/temps serve', paths)).toBe(false)
    expect(isLocalServerCommand('', paths)).toBe(false)
  })
})
