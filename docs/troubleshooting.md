# Troubleshooting

Common issues with symptoms, causes, and fixes.

## 1. Plugin shows "Looking for server on :7575..."

**Symptom:** The status dot pulses blue and the welcome screen is visible.

**Cause:** The Vertigo Sync server is not running, or it is running on a different port.

**Fix:**

```bash
# Start the server
vsync --turbo serve

# Or check if something else is using port 7575
lsof -i :7575
```

If you need a different port:

```bash
vsync --port 8080 serve
```

Note: the Studio plugin currently expects port 7575. Custom port support in the plugin is planned.

## 2. Plugin connects but files do not sync

**Symptom:** Green "Connected" status, but edits to `.luau` files do not appear in Studio.

**Cause:** The file is outside the mapped source paths, or the file watcher missed an event.

**Fix:**

1. Verify the file is under a mapped path (`src/Server/`, `src/Client/`, `src/Shared/`, or `Packages/`)
2. Check that `default.project.json` has the correct `$path` mappings
3. Force a resync: click the "Resync" button in the toolbar, or restart the server

## 3. "Snapshot sync failed: malformed payload"

**Symptom:** Error in Studio output, status shows red.

**Cause:** The server returned an unexpected response format. This can happen if another service is running on port 7575.

**Fix:**

```bash
# Check what is running on port 7575
curl http://127.0.0.1:7575/health

# Expected response: JSON with server version and status
# If you get HTML or a different response, another service is using the port
```

## 4. "WS client lagged; requesting snapshot resync"

**Symptom:** Warning in Studio output, followed by a full snapshot reconciliation.

**Cause:** The WebSocket message buffer overflowed because Studio could not process updates fast enough. This can happen during large batch file operations.

**Fix:** This is normal behavior during bulk operations (e.g., git checkout, branch switch). The plugin automatically resyncs. If it happens frequently during normal editing:

1. Check Studio frame rate -- heavy scenes reduce the apply budget
2. Reduce the number of concurrent file watchers or batch your file operations

## 5. "Health check failed (attempt=N)"

**Symptom:** Status turns red after several failed health checks.

**Cause:** The server became unreachable. Common reasons:
- Server process crashed
- Network interruption (if not running on localhost)
- Server ran out of file descriptors

**Fix:**

```bash
# Check if the server is still running
curl http://127.0.0.1:7575/health

# If not running, restart it
vsync --turbo serve

# Check server logs for crash information
```

## 6. "Apply queue overflow -- forcing resync"

**Symptom:** Warning in Studio output, all pending operations are dropped and a full resync occurs.

**Cause:** More than 2048 file operations queued up faster than Studio could apply them. This usually means a very large number of files changed simultaneously.

**Fix:** This is a safety mechanism. The resync will restore correct state. If it happens repeatedly:

1. Avoid operations that touch thousands of files simultaneously
2. Consider breaking large refactors into smaller batches

## 7. WebSocket disconnects frequently

**Symptom:** Status alternates between "Connected" and "Reconnecting..." with yellow pulsing dot.

**Cause:** The WebSocket connection is being interrupted. Common reasons:
- Firewall or antivirus interference
- Studio HTTP settings blocking WebSocket upgrades
- Server restart during development

**Fix:**

1. The plugin automatically falls back to SSE or HTTP polling -- sync continues
2. Check Studio's HTTP settings: Edit > Studio Settings > Security > "Allow HTTP Requests" must be enabled
3. If using a corporate network, WebSocket may be blocked; the plugin will use polling instead

## 8. "Diff base fingerprint mismatch"

**Symptom:** Warning in Studio output, followed by a full snapshot resync.

**Cause:** The server's diff history does not contain the fingerprint the plugin expected. This happens when the server restarts or its history buffer is exceeded.

**Fix:** This is normal recovery behavior. The plugin automatically resyncs from a full snapshot. No action needed.

## 9. Files appear with wrong script type

**Symptom:** A `ModuleScript` appears as a `Script`, or vice versa.

**Cause:** The file naming convention determines the script type:
- `*.server.luau` / `init.server.luau` -> `Script`
- `*.client.luau` / `init.client.luau` -> `LocalScript`
- Everything else -> `ModuleScript`

**Fix:** Rename the file to match the intended script type. For example, rename `MyService.server.luau` to `MyService.luau` if it should be a `ModuleScript`.

## 10. Validation errors on startup

**Symptom:** `vsync validate` reports errors.

**Cause:** Source files have issues detected by the built-in linter. Common issues:
- Missing `--!strict` directive
- Missing `@native` on hot-path functions
- Usage of deprecated APIs
- `Instance.new()` calls inside Heartbeat connections

**Fix:** Address the reported issues. The validation output includes file paths, line numbers, and rule names:

```bash
vsync validate
```

Example output:

```
src/Server/Services/MyService.luau:1: error: missing --!strict directive [strict-mode]
src/Client/Controllers/Input.luau:42: warning: Instance.new in hot path [hot-path-alloc]
```

## 11. `plugin-install` refuses to install the generated plugin

**Symptom:** `vsync plugin-install` exits with plugin safety errors.

**Cause:** Vertigo Sync now validates the generated Studio plugin before install. It fails closed when the bundle exceeds the top-level symbol budget, a function looks likely to hit Studio's local/register limits, or Luau compiler checks fail.

**Fix:**

1. Run `vsync validate` and inspect the `plugin safety` section
2. Reduce top-level declarations by namespacing helpers or consolidating constants
3. Split or simplify the highest-risk function reported by the validator
4. Retry `vsync plugin-install` once the safety report is clean

## 12. Scan a Studio log for fatal plugin/runtime failures

**Symptom:** You want a fast local check for the exact Studio failure signatures that static validation can miss.

**Fix:**

```bash
vsync plugin-smoke-log --log ~/Library/Logs/Roblox/<your-log>.log
```

For hermetic CI or local harness runs, pass the exact external plugins you expect:

```bash
vsync plugin-smoke-log --log ~/Library/Logs/Roblox/<your-log>.log \
  --allow-plugin user_VertigoSyncPlugin.lua \
  --allow-plugin user_MCPStudioPlugin.rbxm
```

When any other `user_` or `cloud_` plugin appears in the log, the command fails closed.

If Roblox injects a managed `cloud_` plugin that you do not control, keep the run hermetic for local plugins but ignore those cloud loads:

```bash
vsync plugin-smoke-log --log ~/Library/Logs/Roblox/<your-log>.log \
  --ignore-cloud-plugins \
  --allow-plugin user_VertigoSyncPlugin.lua \
  --allow-plugin user_MCPStudioPlugin.rbxm
```

The command fails closed on patterns such as:

- `Out of local registers`
- `attempt to call a nil value`
- `Write apply permanently failed`
- `Snapshot sync failed` (excluding known benign localhost connect-fail noise)

## Diagnostic Commands

```bash
# Server health
curl http://127.0.0.1:7575/health

# Current snapshot
curl http://127.0.0.1:7575/snapshot | jq '.fingerprint, (.entries | length)'

# Validation report
curl http://127.0.0.1:7575/validate | jq '.'

# Prometheus metrics
curl http://127.0.0.1:7575/metrics

# Server configuration
curl http://127.0.0.1:7575/config

# Full determinism check
vsync doctor
```

## Studio Output Filtering

To see only Vertigo Sync messages in Studio output, filter for the `[VertigoSync]` prefix:

```
[VertigoSync] Plugin initialized. version=2026-03-16-v9 mode=edit ws=available
[VertigoSync] Snapshot reconciled (bootstrap). fingerprint=a3f8c2... entries=529
```

## Getting Help

If none of the above resolves your issue:

1. Run `vsync doctor` and include the output
2. Check Studio output for `[VertigoSync]` messages
3. Include the output of `curl http://127.0.0.1:7575/health`
4. File an issue with the above diagnostics
