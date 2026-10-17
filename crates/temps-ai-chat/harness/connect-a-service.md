---
name: connect-a-service
description: Connect a database, cache or object store to a project's app, and offer it whenever a project and a service are made together
contexts: assistant, project
---

# Connect a service to a project

Use this when the user asks to connect, use or wire up a database, cache or
object store (PostgreSQL, Redis, MongoDB, S3) for a project, or asks whether the
app is already connected. Also offer it yourself, without being asked, as soon as
a chat has made both a project and a service. Never answer "no" and stop: say what is missing and
offer to do it.

Linking is the connection. A service linked to a project hands the app its
connection settings itself on every deployment. Do not copy connection details
into the project's environment variables, do not ask the user to type or paste a
password, and do not ask them to name the database: Temps provisions one for the
project when it is linked.

1. Link the service to the project with the link operation. If the service does
   not exist yet, create it first. Say which service and which project.
2. Tell the app which variables to read. They appear in the app's environment:
   - PostgreSQL: `POSTGRES_URL`, or `POSTGRES_HOST`, `POSTGRES_PORT`,
     `POSTGRES_DB`, `POSTGRES_USER` and `POSTGRES_PASSWORD`.
   - Redis: `REDIS_URL`, or `REDIS_HOST`, `REDIS_PORT` and `REDIS_PASSWORD`.
   - MongoDB: `MONGODB_HOST`, `MONGODB_PORT`, `MONGODB_DATABASE`,
     `MONGODB_USERNAME` and `MONGODB_PASSWORD`.
   - S3: `S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` and
     `S3_REGION`; the `AWS_*` equivalents are set too.
3. Change the app to use them: add the client library, read the variable, and
   run one small query or call so the page shows a real value. Do this in the
   sandbox when you have one. The sandbox does not receive these variables, so
   build and check it there, and prove the connection on the deployed app.
4. The variables reach the app on the next deployment, so deploy the project
   again after linking and after the code change. To get the code to the
   project, follow the `build-an-app` playbook.
5. Check the deployed page or endpoint and report what it returned. If it
   cannot connect, say which variable was missing or which error came back.

Only ask the user for a decision that is theirs, such as which service to use
when there are several. Do not hand a step back to them that Temps can do.
