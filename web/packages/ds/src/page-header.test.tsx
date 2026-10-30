// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { PageNamedByChromeContext } from './page-context'
import { PageContainer, PageHeader } from './page-header'

test('a page container is full width unless asked otherwise', () => {
  const html = renderToStaticMarkup(<PageContainer>Body</PageContainer>)
  expect(html).toContain('data-page-width="full"')
  expect(html).not.toContain('max-w-')
})

test('a form page is a capped, centered column', () => {
  const form = renderToStaticMarkup(
    <PageContainer width="form">Body</PageContainer>
  )
  expect(form).toContain('data-page-width="form"')
  expect(form).toContain('mx-auto max-w-5xl')
})

test('a page container inside another adds no second gutter or cap', () => {
  const html = renderToStaticMarkup(
    <PageContainer width="form">
      <PageContainer width="full">Body</PageContainer>
    </PageContainer>
  )
  expect(html.match(/data-page-container[=\s>]/g)?.length).toBe(1)
  expect(html).toContain('data-page-container-nested')
  expect(html.match(/px-4 py-6/g)?.length).toBe(1)
})

test('a title the chrome already shows is kept for screen readers only', () => {
  const html = renderToStaticMarkup(
    <PageNamedByChromeContext.Provider value={true}>
      <PageHeader title="Metrics" description="Explore metrics." />
    </PageNamedByChromeContext.Provider>
  )
  expect(html).toContain('<h1 class="sr-only">Metrics</h1>')
  expect(html).toContain('Explore metrics.')
})

test('section headings are never hidden by the chrome', () => {
  const html = renderToStaticMarkup(
    <PageNamedByChromeContext.Provider value={true}>
      <PageHeader headingLevel="h2" title="Builds" />
    </PageNamedByChromeContext.Provider>
  )
  expect(html).toContain('<h2 class="text-2xl font-semibold tracking-tight">')
})
