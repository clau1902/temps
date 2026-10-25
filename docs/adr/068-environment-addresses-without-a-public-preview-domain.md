# ADR 068: Environment addresses without a public preview domain

## Status

Proposed — 2026-10-25

Related: ADR 018 (stable environment hostnames), ADR 031 (managed DNS records
and the ownership marker), ADR 058 (ingress modes, D8 provider registry, D9
tunnel choice per hostname, D10 tunneled domains), ADR 064 (making a project
reachable: D3 per-address reachability, D4 plan/run journal, D7 checks),
ADR 065 (tailnet names).

## Context

Every hosted environment gets one managed environment domain when it is
created: an `environment_domains` row whose `domain` is the environment's
label (`environment.subdomain`, e.g. `hello-temps-production`), not a full
hostname (`EnvironmentService::create_environment`,
`reconcile_managed_domain`). Routing, reachability and monitors expand that
label to `<label>.<preview_domain>` (`PublicHostnameStrategy::environment_hostname`,
`route_table.rs`, `ReachabilityService::ways_in`).

`preview_domain` defaults to `localho.st` (`AppSettings::default`), a name that
resolves to 127.0.0.1. Until an operator sets a wildcard domain, every
environment's only generated address works on nobody's machine but the one
that opens it. Reachability already says so: `is_local_only` drops the address
from the ways in, and the header reads "Deployed on master, but nobody can
reach it yet: … only resolves to the computer it is opened on" (`compute.rs`).

Other surfaces do not ask reachability:

- **The uptime monitor** probes `<label>.<preview_domain>`
  (`MonitorService::populate_monitor_url` → `get_deployment_url_by_slug`).
  The outbound policy refuses loopback, so every check fails, and the project
  shows a red "Uptime down" dot for an app that is up behind a tunnel.
- **The open-in-new-tab button** opens `environment.domains[0]`
  (`resolveStableUrl` in `web/src/lib/deployment-url.ts`), which can be the
  loopback name.
- **A deploy reports success** with no word that nobody can open the result.
  Nothing records a warning on a deployment; the only per-install nudge is the
  "Add a wildcard domain" step of the setup checklist (`useGettingStarted.ts`).

Meanwhile an install can have a working way in that the environment domain
never uses: a Cloudflare tunnel (ADR 058). A connector knows its zone
(`ingress_connectors.domain`), and with `auto_publish` it publishes every
worker-served hostname under that zone (`ingress_publications`). But the
environment domain lives under `localho.st`, not under the tunnel's zone, so
it is never published. Users then add a custom domain per app by hand
(`app4.autoship.so`), and the environment domain stays a dead row.

Facts this decision relies on:

- A Cloudflare Universal SSL certificate covers the zone apex and one label
  below it (`*.example.com`), not deeper names. Temps already has the
  `Flat` hostname strategy for exactly this (`public_hostname.rs`).
- A tunnel publication writes a DNS record in the zone; ADR 031's ownership
  marker decides what Temps may change or remove.
- `production_hostname_template` and `environment_hostname_template`
  (ADR 058 D9) already shape generated labels (`{project}`, `{env}`).

## Decision

### D1. The environment domain is never lost

The managed environment domain row is the environment's identity, and it is
kept for the life of the environment whatever happens to addressing. Changing
`preview_domain`, adding or removing a tunnel, or adding custom domains never
deletes or renames it; only renaming or deleting the environment does, as
today. What changes is how that label is *served*: under the preview domain
(D2) and, when the preview domain cannot be reached, under a tunnel's zone
(D3).

### D2. A preview domain is usable or it is not

The control plane classifies `preview_domain`, from facts it already has:

| Status | Meaning | Source |
|---|---|---|
| `local_only` | `localhost`, `localho.st` or a name under them | `is_local_only` |
| `unresolved` | a probe label under it (`temps-probe-<random>.<preview_domain>`) does not resolve | DNS, as ADR 064 D7 |
| `no_way_in` | it resolves, but to no address a listener of this install serves publicly | ADR 064 ways in |
| `usable` | it resolves to a public way in of this install | ADR 064 ways in |

The status is shown next to the setting, and it is the "valid preview domain"
every later decision refers to. It is recomputed when the setting, the nodes'
public ingress or the tunnels change, and every 30 minutes.

### D3. Tunnel environment hosts: the environment domain under a tunnel's zone

When the preview domain is not `usable`, a tunnel can carry the environment
domain:

- **Opt-in per tunnel.** A new connector column, `environment_hosts`
  (default `false`), says "give every environment a hostname under this
  tunnel's zone". Existing connectors are unchanged. The console offers to turn
  it on in the D5 warning and when a tunnel is created on an install whose
  preview domain is `local_only`. At most one connector has it on.
- **The name.** `<label>.<connector.domain>`, where `<label>` is the
  environment's existing label (or `production_hostname_template` for
  production). One label below the zone, so Universal SSL covers it with no
  certificate step. The `Flat` rule applies to service hostnames
  (`{env}--{service}.<zone>`).
- **The row.** A second `environment_domains` row holding the full hostname,
  with a new column `generated_by_connector_id` naming the connector. That
  column is what marks it as Temps-generated: only those rows are renamed or
  removed by the rules below; a domain a user added is never touched.
- **Publication.** The host is published through that connector by the
  existing ADR 058 path: `auto_publish` connectors pick it up because it is
  under their zone; others get an `ingress_hostname_choices` row for it. DNS
  follows ADR 031: a name already held by a record Temps does not own is a
  conflict. The environment then gets `<label>-<hash>.<zone>` (the existing
  short-hash rule), and the console says why.
- **Lifecycle.**
  - Environment renamed: the tunnel host is renamed with it, new one published
    before the old one is withdrawn.
  - Environment deleted: unpublished and its record removed (owned records
    only).
  - `environment_hosts` turned off, or the connector deleted: the generated
    rows are withdrawn after the console lists them and the user confirms.
  - Preview domain becomes `usable` later: the tunnel hosts stay. A working
    address is never withdrawn by a change elsewhere. New environments stop
    getting one, and the console offers to remove the existing ones.
- **Preview (branch) environments** get one too: branch names become public
  DNS names and enter Certificate Transparency logs, which the opt-in says.
  `environment_hostname_template` can shorten them (open question 2).

Self-hosted installs that turn nothing on keep exactly today's behaviour: the
label under `localho.st`, nothing published.

### D4. One primary address per environment

Reachability (ADR 064 D3) names one **primary address** per environment: the
first reachable one, in this order:

1. a custom domain;
2. a tunnel environment host (D3);
3. the generated preview-domain host;
4. a private address (ADR 064 D5, ADR 065), for people on that network.

Every surface that means "the app's address" uses it: the header, the
open-in-new-tab button, the deployment page, notifications, and the uptime
monitor. `environment.domains` in the API lists the primary address first.

With no reachable address:

- the open button is disabled with the reason;
- the uptime monitor does not probe. It reports `no_address` ("nothing to
  check: nobody can reach this environment yet"), drawn grey, never `down`;
- the monitor resumes on the next primary address without being recreated.

A monitor whose check path was set by the user keeps that path on the new
address. A monitor created by hand with an explicit URL is not changed.

### D5. Warn when a deploy produces an app nobody can open

A successful deployment of a hosted environment carries a **reachability
warning** when, at completion:

- the environment has no reachable address (D4), **and**
- the preview domain is not `usable` (D2), **and**
- no tunnel environment host (D3) or custom domain serves it.

Excluded: projects hosted elsewhere (`hosting = external`), background workers
with no port, and environments with a reachable private address.

The warning never fails or pauses the deploy; the deployment status stays
successful. It appears in four places, all from one computed value (no new
table):

1. **The deploy log.** `mark_deployment_complete` writes a final block:

   ```
   ⚠ Deployed, but nobody can reach hello-temps-production yet:
     hello-temps-production.localho.st only resolves to this computer.
     Make it reachable:
       • publish it through tunnel 'autoship-so' as hello-temps-production.autoship.so
       • or set a wildcard preview domain in Settings → Platform
       • or add a domain to this environment
   ```

   The options listed are the ones that fit this install, in that order: a
   tunnel that could carry it (D3), the preview domain, a custom domain, and
   public ingress on a worker (with the exact `temps agent ingress` command)
   when no node takes public traffic.
2. **The deployment and project APIs.** A `reachability_warning` field
   (`null` when there is none) with the message and the fixes, so the console
   and `temps deploy` print the same words.
3. **The console.** A banner on the deployment page and the project header
   (which already shows the ADR 064 summary) with one button per fix. The
   tunnel fix runs the D3 opt-in through the ADR 064 plan and run, so it is
   undoable.
4. **One notification per environment** (`batch_key`
   `reachability:<environment_id>`), sent when the warning first appears and
   again only after it cleared and came back. No notification per deploy.

The setup checklist's "Add a wildcard domain" step becomes "Make your apps
reachable". It is done when the preview domain is `usable` or a tunnel has
`environment_hosts` on.

### D6. What is out of scope

- Callback and webhook URLs (`public_ingress_url`, `external_url`) keep their
  own setting and warning. A tunnel host for the console itself is a separate
  decision.
- How a managed service (Temps Cloud) supplies a preview domain to the
  installs it hosts is decided on that side. This ADR only requires that a
  supplied domain goes through D2 like any other.

## Security

- D3 publishes environment and branch names as public DNS names, and their
  certificates appear in Certificate Transparency logs. It is opt-in per
  tunnel, and the opt-in says so.
- D3 never overwrites a DNS record Temps does not own (ADR 031) and never
  touches an `environment_domains` row it did not generate
  (`generated_by_connector_id`).
- A tunnel environment host is public. Turning it on for an environment whose
  addresses are `private` or `signed_in` (ADR 064, ADR 065) is refused: those
  audiences are set per address, and a generated host must not widen them.
- The D4 monitor change removes a probe; it adds no outbound request. The
  `no_address` state is not a way to silence a real outage: it is only set
  when reachability reports no reachable address.
- The D5 warning carries hostnames and node names the viewer can already see.
  Notifications follow the existing preferences.

## Consequences

- A deploy never ends in a silent success that nobody can open.
- An install with a tunnel and no wildcard domain gets a working address for
  every environment with one setting, instead of a custom domain per app.
- The uptime dot stops reporting "down" for apps that are up but have no
  address it can probe.
- `environment_domains` gains rows holding full hostnames generated by Temps,
  marked by `generated_by_connector_id`; code that assumed every row is either
  a label or a user domain has to read that column.

## Phases

| Phase | Scope |
|---|---|
| 1 | D2 preview-domain status; D4 primary address in the header, open button, deployment page and API ordering; the monitor's `no_address` state; D5 warning in the log, the API, the console and notifications; the checklist step |
| 2 | D3 tunnel environment hosts: `environment_hosts` and `generated_by_connector_id` columns, generation, publication, renames, withdrawal, and the one-click fix from the D5 warning |

## Open questions

1. Should D3 also apply to environments created before the opt-in, all at
   once, or only on their next deploy? The proposal: all at once, listed and
   confirmed in the console first.
2. Do preview (branch) environments get tunnel hosts by default, or only
   production and named environments? Branch names leak into public DNS.
3. With several tunnels, is "at most one with `environment_hosts`" enough, or
   should a project choose its tunnel?
4. Should the D5 warning also fire when the only way in is private (a tailnet
   name), for an environment whose audience is `public`?
