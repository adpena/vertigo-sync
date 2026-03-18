# Quickstart

Zero to syncing in 60 seconds.

## Step 1: Install

```bash
cargo install --path .
```

You should see cargo compile the crate and install the `vsync` binary.

```
  Installing vertigo-sync v0.1.0
   Compiling vertigo-sync v0.1.0
    Finished release [optimized] target(s) in 25.74s
  Installing /Users/you/.cargo/bin/vsync
   Installed package `vertigo-sync v0.1.0`
```

## Step 2: Start the Server

From your project root:

```bash
vsync serve
```

You should see the server announce its listening address and selected project, for example:

```
Vertigo Sync v0.1.0
  Server: http://127.0.0.1:7575
  WebSocket: ws://127.0.0.1:7575/ws
  Mode: turbo (10ms coalesce)
  Project: /Users/you/my-roblox-project/default.project.json
  Watching: src
```

## Step 3: Open Studio

Open Roblox Studio. The Vertigo Sync plugin connects automatically.

You will see:

1. A green status dot in the Vertigo Sync panel
2. "Connected  ·  ws  ·  #a3f8c2" in the status line

If the server is not running, the plugin shows a welcome screen with setup instructions and a "Check Connection" button.

## Verify It Works

Edit any `.luau` file in your project. Within milliseconds, you will see:

- The file update in Studio
- The fingerprint hash updating in the status line

## Next Steps

- [Migration from Rojo](migration-from-rojo.md) -- if you are switching from Rojo
- [Configuration Reference](configuration.md) -- all CLI flags and project settings
- [Troubleshooting](troubleshooting.md) -- common issues and fixes
