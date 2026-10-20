// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { MemoryRouter } from 'react-router'

import { PermissionCard, type PermissionRequest } from './PermissionCard'

function renderPermission(input: Record<string, unknown>) {
  const permission: PermissionRequest = {
    id: 'permission-1',
    kind: 'tool_approval',
    tool_name: 'temps_write',
    input,
  }
  return renderToStaticMarkup(
    <MemoryRouter>
      <PermissionCard
        conversationPublicId="conversation-1"
        permission={permission}
        userScoped
      />
    </MemoryRouter>
  )
}

describe('PermissionCard rich platform writes', () => {
  test('renders a native database creation approval instead of raw JSON', () => {
    const markup = renderPermission({
      operation: 'create_service',
      method: 'POST',
      summary: 'Create a PostgreSQL database',
      parameters: {
        name: 'app-db',
        service_type: 'postgres',
        version: '18',
        project_id: 42,
        parameters: {
          database: 'app',
          username: 'postgres',
          password: '********',
        },
      },
    })

    expect(markup).toContain('Create PostgreSQL')
    expect(markup).toContain('aria-label="PostgreSQL logo"')
    expect(markup).toContain('Target service')
    expect(markup).toContain('Service 42')
    expect(markup).toContain('Approve &amp; run')
    expect(markup).not.toContain('password')
    expect(markup).not.toContain('parameters:')
  })

  test('renders a native existing-database link approval', () => {
    const markup = renderPermission({
      operation: 'link_service_to_project',
      method: 'POST',
      parameters: { id: 7, project_id: 42 },
    })

    expect(markup).toContain('Link existing database')
    expect(markup).toContain('Database 7')
    expect(markup).toContain('Service 42')
    expect(markup).toContain('refresh workspace networking')
    expect(markup).not.toContain('parameters:')
  })
})

describe('PermissionCard generic platform writes', () => {
  test('lists the parameters readably and hides credentials', () => {
    const markup = renderPermission({
      operation: 'create_s3_source',
      method: 'POST',
      summary: 'Create a new S3 source (POST /backups/s3-sources)',
      parameters: {
        name: 'temps-backups',
        bucket_name: 'temps-backups',
        force_path_style: true,
        access_key_id: '***',
        secret_key: '***',
      },
      carries_handle: true,
      required_permission: null,
      action_id: 'abc123',
    })

    expect(markup).toContain('Create a new S3 source')
    expect(markup).not.toContain('POST /backups')
    expect(markup).toContain('Bucket name')
    expect(markup).toContain('temps-backups')
    expect(markup).toContain('Yes')
    expect(markup).toContain('Hidden, provided by you')
    expect(markup).toContain('the assistant never saw them')
    expect(markup).not.toContain('carries_handle')
    expect(markup).not.toContain('action_id')
    expect(markup).not.toContain('{&quot;name&quot;')
    expect(markup).toContain('Approve &amp; run')
  })
})
