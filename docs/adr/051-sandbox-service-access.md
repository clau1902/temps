# ADR-051: Sandbox access to linked services

**Status:** Proposed
**Date:** 2026-10-17
**Author:** David Viejo
**Builds on:** ADR-013 (sandbox egress credential proxy, "Phase 2: linked-service credentials"), ADR-036 (persistent workspace sandboxes)

---

## Context

A chat that builds an app in its sandbox also creates the project and the
services the app needs (a PostgreSQL database, an object store). Today the two
halves cannot meet:

1. **The network path exists, but chats never use it.** A sandbox sits on an
   isolated bridge with a per-sandbox egress proxy (ADR-013). For sandboxes
   attached to a project, `SandboxService::synchronize_data_network` already
   builds a per-sandbox *internal* network that contains the sandbox and the
   database containers of the project's linked services, and nothing else. A
   chat sandbox is created without a project unless the chat is a project
   chat, and the assistant chat that creates a project and its database never
   attaches either, so the path is never built.
2. **No scoped credential.** The credentials Temps hands a deployment
   (`get_runtime_env_vars`) name a database per project environment
   (`<project>_<environment>`) but authenticate as the cluster's administrator
   (`postgres`) with the administrator's password. Whoever holds them can read
   every other database on the same cluster. That is acceptable for a deployed
   app built from reviewed code; it is not acceptable for a sandbox that runs
   whatever the model writes. `SandboxService::runtime_environment` and its
   endpoint (`POST /v1/sandboxes/{id}/runtime-environment`) hand exactly those
   production credentials to a sandbox today.
3. **Environment parity is left to the user.** The assistant has told users to
   copy connection details into environment variables by hand, and to type
   passwords themselves. Moving between "local" (the sandbox) and "production"
   is where configuration usually breaks, and nothing in Temps owns that move.

## Decision

### 1. One contract across environments

The app reads the same, fixed variable names everywhere (`POSTGRES_URL`,
`POSTGRES_HOST`, `POSTGRES_PORT`, `POSTGRES_DB`, `POSTGRES_USER`,
`POSTGRES_PASSWORD`, and the equivalents of other service types). Code never
changes between environments. **Only the values change, and only the
environment decides them:**

| Where the code runs | Database it gets | Role it connects as |
|---|---|---|
| Sandbox | `<project>_sandbox`, its own database | a scoped role for that database only |
| Production | `<project>_production` (unchanged) | unchanged for now |
| Other environments | as today | as today |

A sandbox **never** receives production credentials and never touches
production data. Data is not copied between environments; the schema travels as
migrations in the repository, which the app runs on start in every environment.
The assistant writes names only (`.env.example`), never values, into the
workspace, so a secret cannot reach Git.

### 2. A scoped role per sandbox database

Provisioning the sandbox database also creates a role that can use that
database and nothing else:

- `NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION`, a connection limit, and
  ownership of the one database, so migrations work without any wider right.
- `CONNECT` on the database is revoked from `PUBLIC` and granted to the role
  only. The same revoke is applied to the other databases Temps manages on the
  cluster, so the role cannot connect to production and the older default
  (`CREATE` on `public`) cannot be used against it.
- The password is derived, not stored: `HMAC-SHA256(admin password,
  "temps-scoped-role-v1:" || service id || ":" || database)`. It is stable
  across calls, so an app keeps working between restarts, and it changes when
  the administrator password does. Provisioning is idempotent and re-syncs the
  role's password on every call.

PostgreSQL ships first. Other service types report "not supported yet" for
sandbox access rather than hand over their shared credentials (Redis: an ACL user
limited to a key prefix; MongoDB: a user on one database; S3: a key restricted
to the one bucket — each its own change).

### 3. The network path is the existing data network

No new container. A connected sandbox is attached to its per-sandbox internal
data network (`synchronize_data_network`), which holds only the database
containers of the services the user connected. The scoped credentials name
those containers as hosts, on the container port, so the same names work
inside the sandbox. A chat's connection set is a list of project ids; the
network is rebuilt from it, so disconnecting also tears down open sockets
(the reconciliation already disconnects the sandbox first).

Supported first: the Docker backend, services running on the control plane. A
service on another node, or another backend, is refused with a message that
says why; it never falls back to the shared credentials.

### 4. Credentials are injected per command, server side

Connections are recorded on the chat as service ids only. On every
`sandbox_exec` the server resolves the variables fresh and passes them in the
command's environment, naming the database containers on the data network. Nothing is baked
into the container or written to a file, so revoking a connection or rotating a
password takes effect on the next command. Values are removed from the output
shown to the model and the transcript. A server started in the background keeps
the values it started with; the assistant restarts it after a change.

### 5. Consent, authorization, audit

Connecting is the user's decision, shown where the work happens: an option when
a sandbox is created for a project chat, and a control on each service the chat
made. It needs the permissions of issuing runtime credentials
(`ExternalServicesWrite`, `SecretsRead`, access to the project) and is audited
with variable names only, never values. The assistant may ask the user to
connect (an offer card, like the sandbox offer); it cannot connect on its own.

## Phases

1. **Provider:** scoped role and sandbox database provisioning for PostgreSQL
   (standalone and cluster), the `SandboxRuntimeCredentialsProvider` seam gains
   a sandbox-scoped issuance, and `runtime_environment` stops handing
   production credentials to a sandbox for PostgreSQL.
2. **Chat link:** the recorded connections (project ids), the data network
   built from them, per-command injection into `sandbox_exec`, output
   redaction, endpoints, audit.
3. **Surface:** the option at sandbox creation, the control on "Made here"
   service rows, the offer card, and a `connect-a-service` playbook that covers
   both environments.
4. **Other service types**, and an opt-in move of production to scoped roles.

## Consequences

- Production behaviour does not change in phases 1 to 4, so nothing that runs
  today is migrated.
- A sandbox database is real PostgreSQL on the user's cluster: it uses disk and
  connections, and survives the sandbox. It is named for the project, so a new
  chat on the same project continues with the same data.
- Moving production to scoped roles needs an ownership migration for existing
  databases and is deliberately a separate, opt-in step.

## Not decided here

- Seeding a sandbox database from production (a masked snapshot).
- Remote nodes and the Firecracker and microsandbox backends.
