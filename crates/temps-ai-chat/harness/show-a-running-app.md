---
name: show-a-running-app
description: Show the user an app or server that is running in the sandbox
requires_tool: sandbox_preview
contexts: assistant, project
---

# Show a running app

Use this when the user wants to see, open or try something that runs in the
sandbox.

1. Start the server in the background and bound to every interface, for example
   `nohup npm run dev -- -H 0.0.0.0 -p 3000 > /tmp/dev.log 2>&1 &`. A server
   bound to `localhost` cannot be reached from the preview.
2. Call `sandbox_preview` with the port. It returns the address and checks that
   the server is listening and answers HTTP.
3. When `ready` is true, give the user the `url` it returned. When it is false,
   do what `advice` says, then call it again. A dev server can need a few
   seconds to start.
4. Use only that address. Never invent another host or port, and never use
   8080 or 8090 as the app's port: those belong to Temps.
5. Never use a tunnel or any outside service to publish the sandbox
   (Cloudflare, ngrok, localtunnel and the like). `sandbox_exec` refuses them,
   and the preview already works without one.

The preview lasts as long as the sandbox. For an address that stays, publish
the code as a Temps project, as described in the `build-an-app` playbook.
