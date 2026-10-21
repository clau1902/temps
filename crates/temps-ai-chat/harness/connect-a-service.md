---
name: connect-a-service
description: Connect a database, cache or object store to a project's app, and offer it whenever a project and a service are made together
contexts: assistant, project
---

# Connect a service to a project

Use this when the user asks to connect, use or wire up a database, cache or
object store (PostgreSQL, Redis, MongoDB, S3) for a project, or asks whether the
app is already connected. When an accepted architecture has a flow from an app
to a database, cache or bucket, link that service as part of deploying, without
being asked. Otherwise offer it yourself as soon as a chat has made both a
project and a service. Never answer "no" and stop: say what is missing and
offer to do it.

Linking is the connection. A service linked to a project hands the app its
connection settings itself on every deployment. Do not copy connection details
into the project's environment variables, do not ask the user to type or paste a
password, and do not ask them to name the database: Temps provisions one for the
project when it is linked.

1. A new service: read `get_service_type_parameters`, then `create_service`
   with the project's `--project_id` creates and links it in one step. An
   existing one: `link_service_to_project`. Say which service and which project.
2. Tell the app which variables to read. They appear in the app's environment:
   - PostgreSQL: `POSTGRES_URL`, or `POSTGRES_HOST`, `POSTGRES_PORT`,
     `POSTGRES_DB`, `POSTGRES_USER` and `POSTGRES_PASSWORD`.
   - Redis: `REDIS_URL`, or `REDIS_HOST`, `REDIS_PORT` and `REDIS_PASSWORD`.
   - MongoDB: `MONGODB_HOST`, `MONGODB_PORT`, `MONGODB_DATABASE`,
     `MONGODB_USERNAME` and `MONGODB_PASSWORD`.
   - S3: `S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` and
     `S3_REGION`; the `AWS_*` equivalents are set too.
3. Change the app to use them: add the client library, read the variable, and
   run one small query or call so the page shows a real value. Build and check
   it in the sandbox when you have one:
   - When the user has connected the sandbox to the project, every
     `sandbox_exec` command already has the PostgreSQL variables above. They
     point at a database of the sandbox's own, empty at first, with a role that
     reaches nothing else; production data is never there. Create the tables
     from the app itself on start (a migration in the repository), so the same
     code works in the sandbox and in production. Never write a value into a
     file, a command line or the chat, and print variable names only. Add a
     `.env.example` that lists names with no values.
   - When it is not connected, the sandbox has none of these variables. Say in
     one sentence that the user can press Connect sandbox on the project in the
     Made here tab, and carry on without asking them for a password. Until
     then, write the code and prove the connection on the deployed app.
   - Only PostgreSQL can be connected to the sandbox so far. For Redis, MongoDB
     and S3, make the code fail with a clear message when the variable is
     missing, test the rest of the logic in the sandbox without it, and prove
     the connection on the deployed app.
4. The variables reach the app on the next deployment, so deploy the project
   again after linking and after the code change. To get the code to the
   project, follow the `build-an-app` playbook.
5. Check the deployed page or endpoint and report what it returned. If it
   cannot connect, say which variable was missing or which error came back.

Only ask the user for a decision that is theirs, such as which service to use
when there are several. Do not hand a step back to them that Temps can do.
