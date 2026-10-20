---
name: add-temps-observability
description: Wire Temps analytics, error tracking and OpenTelemetry traces into an app you build or change
requires_tool: sandbox_exec
contexts: assistant, project
---

# Add Temps analytics, error tracking and traces

Every app Temps deploys is handed what it needs to report to its own
project: visitor analytics, error tracking and traces. Nothing arrives until
the app's code uses it, so wire it in while you build, before the first
deploy. Do it whenever the stack has a recipe below. For any other stack, use
its official Sentry and OpenTelemetry SDKs the same way, or tell the user
which of the three you left out and why.

## What Temps sets on every deployment

- `SENTRY_DSN`: a Sentry-compatible DSN for the project's environment. Temps
  speaks the Sentry protocol, so use the official Sentry SDK; there is no
  Temps error SDK.
- The browser copy of the DSN, under the prefix the preset's bundler exposes:
  `NEXT_PUBLIC_SENTRY_DSN` for `nextjs`, `VITE_SENTRY_DSN` for `vite`,
  `react` and `vue`, `PUBLIC_SENTRY_DSN` for `sveltekit` and `astro`. It is
  set at build time too, so bundlers inline it. It exists only when the
  project's preset matches the framework.
- `SENTRY_RELEASE`: the release. Never set `release` yourself.
- `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL`
  (`http/protobuf`), `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_SERVICE_NAME`:
  the standard OpenTelemetry exporter settings. The headers hold a deployment
  token: read them only on the server, never log them or send them to a
  browser.
- Analytics needs no variable: Temps answers `/api/_temps/*` on the app's own
  domain, and `@temps-sdk/react-analytics` sends there by default. Add no route,
  key or `basePath` for it.

None of these exist in the sandbox. Start each SDK only when its variable is
set (`enabled: !!dsn`, tracing only with an endpoint), so the app runs and
builds without them. Never hardcode, invent or ask the user for a value.

## Next.js (App Router)

`npm install @sentry/nextjs @vercel/otel @temps-sdk/react-analytics`

`instrumentation.ts`, next to `app/` (in `src/` when the app uses `src/`):

```ts
import * as Sentry from '@sentry/nextjs'
import { registerOTel } from '@vercel/otel'

export function register() {
  Sentry.init({
    dsn: process.env.SENTRY_DSN,
    enabled: !!process.env.SENTRY_DSN,
    // Traces go to Temps through @vercel/otel. Without this, Sentry takes
    // the tracer and no span is exported.
    skipOpenTelemetrySetup: true,
    tracesSampleRate: 0,
  })
  registerOTel({ serviceName: process.env.OTEL_SERVICE_NAME || 'web' })
}

export const onRequestError = Sentry.captureRequestError
```

`instrumentation-client.ts`, in the same folder:

```ts
import * as Sentry from '@sentry/nextjs'

Sentry.init({
  dsn: process.env.NEXT_PUBLIC_SENTRY_DSN,
  enabled: !!process.env.NEXT_PUBLIC_SENTRY_DSN,
  tracesSampleRate: 0,
})
```

In `app/layout.tsx`, which stays a server component, wrap the body:

```tsx
import { TempsAnalyticsProvider } from '@temps-sdk/react-analytics'
// …
<body>
  <TempsAnalyticsProvider>{children}</TempsAnalyticsProvider>
</body>
```

Add `app/global-error.tsx` as a client component that calls
`Sentry.captureException(error)` in a `useEffect` and renders a short message
with a retry button. Do not wrap `next.config` in `withSentryConfig`: it is
only needed to upload source maps, and it can fail the build.

## React single-page app (Vite)

`npm install @sentry/react @temps-sdk/react-analytics`

In `src/main.tsx`:

```tsx
import * as Sentry from '@sentry/react'
import { TempsAnalyticsProvider } from '@temps-sdk/react-analytics'

Sentry.init({
  dsn: import.meta.env.VITE_SENTRY_DSN,
  enabled: !!import.meta.env.VITE_SENTRY_DSN,
})

createRoot(document.getElementById('root')!).render(
  <TempsAnalyticsProvider>
    <App />
  </TempsAnalyticsProvider>,
)
```

Deploy it with the `vite` preset so `VITE_SENTRY_DSN` is set when it builds.
It has no server, so it has no traces; the API it calls traces its own
requests.

## Node server (Express, Fastify, Hono, plain `http`)

`npm install @sentry/node @opentelemetry/sdk-node @opentelemetry/exporter-trace-otlp-proto @opentelemetry/auto-instrumentations-node`

`instrument.cjs`, loaded before the app with
`node --require ./instrument.cjs server.js` in the `start` script (this works
for an ES-module server too):

```js
const Sentry = require('@sentry/node')
Sentry.init({
  dsn: process.env.SENTRY_DSN,
  enabled: !!process.env.SENTRY_DSN,
  skipOpenTelemetrySetup: true,
  tracesSampleRate: 0,
})

if (process.env.OTEL_EXPORTER_OTLP_ENDPOINT) {
  const { NodeSDK } = require('@opentelemetry/sdk-node')
  const { OTLPTraceExporter } = require('@opentelemetry/exporter-trace-otlp-proto')
  const { getNodeAutoInstrumentations } = require('@opentelemetry/auto-instrumentations-node')
  const sdk = new NodeSDK({
    traceExporter: new OTLPTraceExporter(),
    instrumentations: [
      getNodeAutoInstrumentations({
        '@opentelemetry/instrumentation-fs': { enabled: false },
        '@opentelemetry/instrumentation-http': {
          ignoreIncomingRequestHook: (req) => req.url === '/healthz',
        },
      }),
    ],
  })
  sdk.start()
  process.once('SIGTERM', () => sdk.shutdown().finally(() => process.exit(0)))
}
```

After the routes, hand errors to Sentry: `Sentry.setupExpressErrorHandler(app)`
for Express, `Sentry.setupFastifyErrorHandler(app)` for Fastify, and
`Sentry.captureException(err)` in `app.onError` for Hono. Keep the health
path in the ignore hook equal to the app's real health route.

## Python (FastAPI, Flask, Django)

Add `sentry-sdk`, `opentelemetry-distro`,
`opentelemetry-exporter-otlp-proto-http` and the framework's instrumentation
(`opentelemetry-instrumentation-fastapi`, `-flask` or `-django`) to the
requirements. Call `sentry_sdk.init(dsn=os.environ.get("SENTRY_DSN"))` before
the app is created; it does nothing without a DSN. Start the server through
`opentelemetry-instrument`, for example
`opentelemetry-instrument uvicorn main:app --host 0.0.0.0 --port $PORT`; it
reads the `OTEL_` variables itself.

## Go

Use `github.com/getsentry/sentry-go` with
`sentry.Init(sentry.ClientOptions{Dsn: os.Getenv("SENTRY_DSN")})` and
`defer sentry.Flush(2 * time.Second)`. For traces, create the exporter with
`otlptracehttp.New(ctx)`, which reads the `OTEL_` variables. Register
`sdktrace.NewTracerProvider(sdktrace.WithBatcher(exporter))` with
`otel.SetTracerProvider`, and serve through `otelhttp.NewHandler(mux, "server")`.
Only build the exporter when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

## Rules

- Plain HTML with no build step and no framework has no SDK to add. Leave it
  as it is, and say that analytics, error tracking and traces are not wired.
- Keep personal data out: leave `sendDefaultPii` off, and never put tokens,
  emails or passwords in events, span names or analytics properties.
- Turn on session replay only when the user asks for it.
- Check before you deploy: the build exits 0 with the SDKs in, and the app
  starts and answers in the sandbox, where none of the variables exist.
- When you finish, say which of the three the app reports. Tell the user they
  appear on the project's Analytics, Errors and Traces pages once the
  deployment has traffic.
