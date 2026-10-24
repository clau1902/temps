# ADR 064: Making a project reachable: who first, then how

## Status

Proposed — 2026-10-24

Related: ADR-020 (every ingress serves every app), ADR 031 (managed DNS
records), ADR 048 (ingress is a node capability), ADR 055 (workers with no
public address), ADR 058 (ingress modes, providers, tunneled domains).

## Context

What a user wants after a deploy is one thing: their app at an address, for
the people who should use it. ADR 058 built the pieces: ingress modes,
providers (Cloudflare Tunnel first), tunneled domains and a per-hostname
publishing state. Putting them together is still the user's job.

What a new user goes through on a control plane whose workers have no
public address, which is every hosted plane and most home labs:

1. They join a worker and deploy a project. The deployment goes green.
   Nothing says that nobody can reach it.
2. To put it on a domain they must know, unprompted, that they need a
   tunnel; create a Cloudflare token with three permissions; add it as a
   DNS provider; run **Settings → Nodes → Expose your apps** (five steps);
   then go to **Add Domain** and pick "through the tunnel". That is about
   ten screens in three parts of the console, plus Cloudflare's.
3. A project's domains live under the project's **Settings**, next to build
   options and variables, although putting the app online is the main thing
   most users come to do.

And one thing cannot be expressed at all: **who** may reach the app. Every
way in is public. A user who wants an app only on their own network (a
tailnet, a LAN, a VPN) must build it by hand, and the obvious attempt fails
silently: a project IP allowlist of `100.64.0.0/10` behind a Cloudflare
Tunnel blocks everyone, because tunnel traffic carries the visitor's
internet address (`CF-Connecting-IP`), never a tailnet one.

Facts this decision relies on:

- The environment's container list
  (`GET /projects/{id}/environments/{env}/containers`) already returns each
  container's `node_name`, so where an environment runs needs no new field.
- A hosted control plane has no address of its own to hand out: its preview
  domain defaults to `localho.st` (ADR 058 D4), and Temps Cloud lists a
  managed subdomain as a later convenience. Today a public address needs the
  user's own domain.
- Cloudflare's dashboard accepts a token template link
  (`https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=…&accountId=*&zoneId=all&name=…`)
  that opens the create-token form with permissions filled in.

## Decision

### D1. One flow, per environment: *Make it reachable*

There is one flow for putting a project's environment online, started from
the project, and it asks the questions in the order the user thinks them:

1. **Who should reach it?** Anyone on the internet, only my private
   network, or anyone who signs in (D6).
2. **At which address?** The hostname, e.g. `myapp.example.com`, or a name
   the chosen way in provides (a tailnet name, D5).
3. **Create.**

Temps works out the rest from what the cluster already has, and asks only
for what it cannot work out (a credential, a choice between two zones).
"How traffic gets in" is never a question by itself: it follows from the
audience and from what the workers have.

The flow is started from:

- the project header, which always shows the environment's state (D3) with
  **Make it reachable** while nothing reaches it;
- the project's Domains page (D2);
- the end of an environment's first successful deployment;
- the AI tools and the CLI, through the same API (D4).

The existing admin pages stay: **Expose your apps** for setting up ingress
for the whole cluster, the global **Domains** page, and **Add Domain** for
choices this flow does not offer (DNS-01 with a specific provider, an
uploaded certificate). They run the same operations underneath; this flow
is the shortest path through them, not a second implementation.

### D2. Domains is part of the project, not its settings

**Domains** moves from the project's Settings group to the project's main
navigation, next to Overview and Deployments. `settings/domains` and
`domains` keep resolving to it, so existing links keep working. The page
shows, per address, its audience, how traffic gets in, and the live state
from D3.

### D3. Every address says whether it is reachable, and why not

Reachability is computed per address, not per node. Under ADR-020 any
ingress serves any app and forwards over the mesh, so "the worker this
runs on has no ingress" is the wrong test. The right one is: does a way in
serve this hostname, and does DNS lead there?

`GET /projects/{id}/environments/{env}/reachability` returns, per address
attached to the environment:

| Field | Example |
|---|---|
| `address` | `myapp.example.com` |
| `audience` | `public`, `private`, `signed_in` |
| `way_in` | `tunnel:homelab`, `direct:edge-1`, `private:master` |
| `state` | `reachable`, `not_reachable`, `checking`, `unknown` |
| `reason` | "No worker takes traffic for this hostname." |
| `action` | what fixes it, with the link or command |

It builds on ADR 058's per-hostname state (`GET /ingress/hostnames`, D9)
and adds the direct and private ways in. With no address at all it returns
`state = not_reachable` and the plan D4 would propose, so the header can
say "Deployed on `master`, but nobody can reach it yet" with one button.

The header shows one line: **Public at `myapp.example.com`**, **Private at
`myapp.internal.example.com` (Tailscale)**, or **Not reachable yet**, with
the reason. A preview domain that only resolves locally (`localho.st`) is
never shown as reachable.

### D4. The plan is computed by the server and applied as one run

- `POST …/reachability/plan` takes `{audience, address?}` and returns the
  steps Temps would take and the fields still needed, in ADR 058 D8's
  descriptor form (`secret`, `dns_provider`, `managed_domain`, `text`), so
  the console renders it with no code per provider.
- `POST …/reachability/apply` takes the plan's answers and starts a run. Its
  steps report progress like **Expose your apps** does today (save the DNS
  provider, create the tunnel, publish the hostname, set DNS, issue the
  certificate, attach the domain, check reachability).
- Every resource a run creates is recorded on the run. **Undo** removes
  exactly those, newest first, and never anything the run found already
  there.
- The steps move from the console (`web/src/components/nodes/ingress-setup.ts`
  runs them in the browser today) to the server, so a closed tab cannot
  leave half a setup, and the CLI and AI tools get the same behaviour.

For the public audience the plan follows what exists:

| What the cluster has | What the user does |
|---|---|
| A tunnel whose domain covers the hostname | Create (1 action) |
| A Cloudflare DNS provider, no tunnel | Confirm "create a tunnel on `master`" (2) |
| Nothing | Paste a Cloudflare token (D8), then Create (3) |
| A worker with `direct` ingress | Create; DNS set or shown (1–2) |

When several fit, the order is: an existing tunnel, an existing direct
ingress with DNS managed by Temps, a new tunnel. HTTP-01 is only offered
when a `direct` ingress takes port 80; it is never labelled recommended
otherwise.

### D5. Private network: a worker ingress that never listens publicly

A new worker ingress mode, `private`, next to ADR 058 D7's `off`, `fixed`
and `delegated`. Like those, it is the worker owner's decision, saved in
`agent.json`:

```
temps agent ingress private --listen tailscale|<IP> [--https-port 443]
temps join … --ingress private --listen tailscale
```

- **What it binds.** Only an address inside `100.64.0.0/10` (Tailscale and
  other CGNAT overlays), RFC 1918, or IPv6 ULA `fd00::/8`. `tailscale`
  resolves to the address of the `tailscale0` interface. The agent refuses a
  public address, `0.0.0.0` and `::`, with what to run instead. The heartbeat
  already reports the host's addresses (ADR 058 D7); the console marks the
  Tailscale one so the user picks it, not types it.
- **Addresses.**
  - *Your own domain*: Temps creates an A/AAAA record pointing the hostname
    at the worker's private address, through the cluster's DNS provider with
    the ownership marker (ADR 031), and issues the certificate with DNS-01.
    HTTP-01 cannot work here and is not offered.
  - *A tailnet name* (`myapp.<tailnet>.ts.net`): through Tailscale Serve,
    registered as an ADR 058 D8 provider with a Tailscale OAuth client stored
    with `EncryptionService`. No domain needed; Tailscale issues the
    certificate. Phase 3.
- **Audience is a property of the address, enforced in routing.** An address
  marked `private` is served only by `private` listeners. The tunnel
  publisher (`worker_public_routes`, ADR 058 D9) never publishes it, even
  under `auto_publish`, and `direct` listeners answer it with 404. DNS alone
  is not the boundary.
- **A second lock.** The run adds an environment IP allow rule for the
  network's range (`100.64.0.0/10` for Tailscale, the chosen LAN range
  otherwise), recorded on the run so undo removes it. It is defence in depth:
  the boundary is the listener and the routing above.
- **Who in the network.** Temps does not manage Tailscale ACLs. The console
  says "everyone your tailnet's access rules allow to reach `master` on
  443".
- **Several audiences per environment.** An environment may have a public
  address and a private one at once; each address carries its own audience.

### D6. Signed-in audience

Anyone who signs in uses a public way in plus an access gate. Which gates
(the project's own protection settings, Cloudflare Access for tunneled
hostnames) is decided in a later ADR. This one reserves the audience value
and the flow step, so adding it is not a redesign.

### D7. Checks run from where they can tell

- **Public:** the control plane dials the hostname as today (ADR 058 D7's
  check, or the tunnel's publishing state).
- **Private, reachable from inside:** the control plane cannot reach a
  tailnet (a hosted plane runs in Temps Cloud), so the worker's agent makes
  the request: HTTPS to the environment's own hostname through its own
  resolver, nothing else. The result is reported in the heartbeat.
- **Private, not reachable from outside:** the control plane resolves the
  hostname and confirms it points only at private addresses and that no
  tunnel record exists for it. Either failing is shown in red: "This
  private address is also published on the internet through tunnel
  `homelab`."
- Every failure names what to do, per the failure policy: missing DNS
  record, certificate not issued yet, the private listener not bound and
  why, the address resolving somewhere else.

### D8. The Cloudflare token step

- The step links to Cloudflare's token template URL with the permissions
  filled in: Zone · Zone · Read, Zone · DNS · Edit, Account · Cloudflare
  Tunnel · Edit. The user picks the account and zone, creates the token and
  pastes it.
- The permission keys for Zone and DNS are documented by Cloudflare. The key
  for Cloudflare Tunnel is verified during implementation; if there is none,
  the link fills in what it can and the checklist says what to add by hand.
- A pasted token is checked straight away: Temps lists its zones and tests
  each permission, and the checklist marks each one present or missing
  before anything is created.

### D9. A tunnel and a private allowlist never combine

The IP access service refuses, with a specific message, an allowlist that
contains only private ranges on an environment whose addresses are all
served through a tunnel or `behind_proxy` ingress, because every visitor
would be blocked ("Visitors through tunnel `homelab` arrive with their
internet address, so `100.64.0.0/10` would block everyone. To limit this app
to your tailnet, make it private instead."). It warns when only some
addresses are affected.

## Security

- `private` ingress never binds a public address; the agent refuses one, and
  the control plane refuses to set one. The worker owner opts in, as for
  every other ingress mode.
- A `private` address is never published by a tunnel and never answered by
  a public listener (D5). This is enforced in the route snapshot, so a
  misconfigured DNS record exposes nothing.
- Changing an address's audience, in either direction, is a sensitive action
  (step-up): it changes who can reach the app. Private to public names what
  becomes public before it happens.
- A public DNS record pointing at a private address reveals that the name
  exists and the internal address. The console says so when the user picks
  their own domain, and offers the tailnet name, which publishes nothing.
- The agent's inside check requests only the environment's own hostnames;
  it takes no URL from the API, so it cannot be used to reach other
  services on the worker's network.
- Undo deletes only what the run recorded as created. Credentials pasted into
  a run are handled as in ADR 058: `EncryptionService`, never returned,
  audited.
- A token is checked with read-only calls before anything is created.

## Consequences

- A user who has deployed sees whether anyone can reach the app, and gets
  there in one to three actions from the project.
- Keeping an app on a tailnet or LAN becomes a supported, checked setup
  instead of something built by hand that fails silently.
- The ingress setup steps move from the browser to the server; **Expose your
  apps** becomes a client of the same runs.
- Self-hosted single-box installs are unchanged: their proxy serves every
  address publicly as today, and `private` is opt-in per worker.
- When Temps Cloud offers a managed subdomain, it becomes one more address
  source for the public audience, and the default when the user has no
  domain.

## Phases

| Phase | Scope |
|---|---|
| 0 | D2 Domains in project navigation; D3 reachability endpoint and the header state, linking to the existing setup pages |
| 1 | D1/D4 *Make it reachable* for the public audience: server-side plan, apply and undo; D8 token template link and token check; entry after first deploy; D9 |
| 2 | D5 `private` worker ingress with your own domain on the private address; D7 inside and outside checks |
| 3 | Tailscale Serve provider: tailnet names, and its identity headers for "only these tailnet users" |
| 4 | D6 signed-in audience |

## Open questions

1. Should phase 2 allow LAN ranges from the start, or only Tailscale until
   the inside check has been exercised on real networks?
2. Do preview environments inherit their parent environment's audience? A
   private app whose previews are public is a likely surprise.
3. Should the outside check for private addresses also run from Temps Cloud
   for hosted planes, so it does not depend on the control plane's own
   network?
