# Edit Preview Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pull generic edit-preview orchestration out of `vertigo` and into `vertigo-sync`, then update `vertigo` and `arnis-roblox` to consume the shared `vertigo-sync` contract.

**Architecture:** Extend `vertigo-sync`'s project schema with a typed `editPreview` block and move the old standalone preview-plugin behavior into the main `VertigoSync` Studio plugin. Keep `vertigo` and `arnis-roblox` limited to project-specific preview builder modules plus project configuration that points at them.

**Tech Stack:** Rust (`serde`, existing `project.rs` tests), Luau Studio plugin runtime, JSON project config, shell harness scripts.

---

### Task 1: Add `editPreview` project schema support in `vertigo-sync`

**Files:**
- Modify: `<project-root>/src/project.rs`
- Test: `<project-root>/src/project.rs`
- Modify: `<project-root>/docs/configuration.md`

- [ ] **Step 1: Write the failing parser test**

Add a test in `src/project.rs` that parses:

```json
{
  "name": "TestProject",
  "vertigoSync": {
    "builders": {
      "roots": ["src/ServerScriptService/StudioPreview/AustinPreviewBuilder.lua"],
      "dependencyRoots": ["src/ServerScriptService/StudioPreview"]
    },
    "editPreview": {
      "enabled": true,
      "builderModulePath": "ServerScriptService.StudioPreview.AustinPreviewBuilder",
      "builderMethod": "Build",
      "watchRoots": ["ServerScriptService.StudioPreview"],
      "debounceSeconds": 0.25,
      "rootRefreshSeconds": 1.0,
      "mode": "edit_only"
    }
  },
  "tree": { "$className": "DataModel" }
}
```

Assert that the parsed `ProjectTree.vertigo_sync.edit_preview` fields all round-trip correctly.

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p vertigo-sync parse_project_edit_preview_config -- --exact`

Expected: FAIL because `editPreview` is not parsed yet.

- [ ] **Step 3: Implement minimal schema support**

Add:
- `VertigoSyncEditPreviewConfig`
- `RawVertigoSyncEditPreviewConfig`
- `edit_preview` field wiring with `serde(rename = "editPreview")`

Parse and normalize:
- `builderModulePath`
- `builderMethod`
- `watchRoots`
- `enabled`
- `debounceSeconds`
- `rootRefreshSeconds`
- `mode`

- [ ] **Step 4: Re-run the targeted test**

Run: `cargo test -p vertigo-sync parse_project_edit_preview_config -- --exact`

Expected: PASS.

- [ ] **Step 5: Update config docs**

Document the new `vertigoSync.editPreview` block in `docs/configuration.md` with one `Build` example and one `BuildNow` example.

### Task 2: Expose `editPreview` in the `/project` payload

**Files:**
- Modify: `<project-root>/src/server.rs`
- Test: `<project-root>/src/server.rs` or existing server tests if present

- [ ] **Step 1: Write the failing server payload test**

Add a server test that returns `/project` JSON for a project containing `editPreview` and asserts the payload includes:
- `vertigoSync.editPreview.enabled`
- `builderModulePath`
- `builderMethod`
- `watchRoots`
- `debounceSeconds`
- `rootRefreshSeconds`
- `mode`

- [ ] **Step 2: Run the targeted server test to verify it fails**

Run: `cargo test -p vertigo-sync project_endpoint_includes_edit_preview -- --exact`

Expected: FAIL because the payload omits `editPreview`.

- [ ] **Step 3: Implement minimal payload wiring**

Thread the parsed `editPreview` config through the existing `/project` response shape without changing unrelated fields.

- [ ] **Step 4: Re-run the targeted server test**

Run: `cargo test -p vertigo-sync project_endpoint_includes_edit_preview -- --exact`

Expected: PASS.

### Task 3: Move generic edit-preview orchestration into `VertigoSyncPlugin`

**Files:**
- Modify: `<project-root>/assets/plugin_src/00_main.lua`
- Modify: `<project-root>/assets/VertigoSyncPlugin.lua`
- Reference: `<vertigo-repo>/studio-plugin/VertigoEditSync.lua`

- [ ] **Step 1: Add a focused plugin regression first**

Add a lightweight Rust or text-level plugin generation test in `vertigo-sync` that asserts the generated plugin source includes:
- `editPreview`
- `builderModulePath`
- `builderMethod`
- `VertigoPreviewLastBuildError`

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p vertigo-sync plugin_source_contains_edit_preview_runtime -- --exact`

Expected: FAIL because the generated plugin does not contain the new subsystem yet.

- [ ] **Step 3: Implement the shared edit-preview subsystem in `00_main.lua`**

Port only the generic parts from `VertigoEditSync.lua`:
- mode gating (`edit_only`)
- config-driven builder resolution
- config-driven method dispatch (`Build` / `BuildNow`)
- debounce
- bounded retry/backoff
- preview status attrs/logging

Do **not** port `vertigo`-specific hardcoded paths.

Keep the subsystem isolated behind a small `EDIT_PREVIEW` config/runtime table instead of smearing locals across the main plugin scope.

- [ ] **Step 4: Regenerate the single-file plugin artifact**

Run the repo’s normal plugin generation path so `assets/VertigoSyncPlugin.lua` matches `assets/plugin_src/00_main.lua`.

- [ ] **Step 5: Re-run the targeted plugin test**

Run: `cargo test -p vertigo-sync plugin_source_contains_edit_preview_runtime -- --exact`

Expected: PASS.

### Task 4: Remove the standalone `VertigoEditSync` dependency from `vertigo`

**Files:**
- Modify: `<vertigo-repo>/default.project.json`
- Delete: `<vertigo-repo>/studio-plugin/VertigoEditSync.lua`
- Docs/update if needed in `<vertigo-repo>/README.md` or dev docs that reference the old plugin

- [ ] **Step 1: Add project config for the shared contract**

Update `vertigo/default.project.json` with:
- `vertigoSync.builders`
- `vertigoSync.editPreview`

Use:
- `builderModulePath = "ServerScriptService.Server.Studio.StudioPreviewBuilder"`
- `builderMethod = "BuildNow"`

- [ ] **Step 2: Remove the old standalone plugin file**

Delete `<vertigo-repo>/studio-plugin/VertigoEditSync.lua`.

- [ ] **Step 3: Update any docs/scripts that still tell users to install or expect `VertigoEditSync`**

Replace them with `VertigoSyncPlugin` / `vsync plugin-install`.

- [ ] **Step 4: Verify no stale references remain**

Run: `rg -n "VertigoEditSync" <vertigo-repo> -S`

Expected: only intentional migration notes, or zero references if fully removed.

### Task 5: Update `arnis-roblox` to use the shared edit-preview contract

**Files:**
- Modify: `<arnis-roblox-repo>/roblox/default.project.json`
- Modify: `<arnis-roblox-repo>/scripts/run_studio_harness.sh`

- [ ] **Step 1: Add `editPreview` config to `arnis-roblox`**

Update `roblox/default.project.json` with:
- `builderModulePath = "ServerScriptService.StudioPreview.AustinPreviewBuilder"`
- `builderMethod = "Build"`
- watch roots matching the current preview/import/shared/server-storage surfaces

- [ ] **Step 2: Harden the harness around `VertigoSync` signals**

Stop depending on the removed standalone plugin. Prefer:
- `[VertigoSync]` project/bootstrap messages
- preview build attrs/logs emitted by the integrated subsystem
- existing Austin preview completion markers

- [ ] **Step 3: Verify no harness logic still assumes `VertigoEditSync`**

Run: `rg -n "VertigoEditSync|StudioPreviewBuilder" <arnis-roblox-repo>/scripts <arnis-roblox-repo>/roblox -S`

Expected: no references to the old standalone plugin contract.

### Task 6: Verify the end-to-end extraction

**Files:**
- Verify across:
  - `<project-root>`
  - `<vertigo-repo>`
  - `<arnis-roblox-repo>`

- [ ] **Step 1: Run `vertigo-sync` tests**

Run: `cargo test --manifest-path <project-root>/Cargo.toml --all-targets --all-features`

Expected: PASS.

- [ ] **Step 2: Reinstall the Studio plugin from `vertigo-sync`**

Run: `cargo run --manifest-path <project-root>/Cargo.toml -- plugin-install`

Expected: installed plugin updated.

- [ ] **Step 3: Start a real `vsync` serve session for `arnis-roblox`**

Run:

```bash
vsync --root <arnis-roblox-repo> --turbo serve --project roblox/default.project.json
```

Expected: server starts and advertises the `arnis-roblox` project identity.

- [ ] **Step 4: Run the `arnis-roblox` edit harness**

Run:

```bash
bash <arnis-roblox-repo>/scripts/run_studio_harness.sh --takeover --hard-restart --no-play --edit-wait 30 --pattern-wait 150
```

Expected:
- `[VertigoSync]` connects
- preview subsystem resolves `AustinPreviewBuilder`
- no `missing Server.Studio.StudioPreviewBuilder` orphan-plugin noise

- [ ] **Step 5: Commit by repo**

Commit separately per repo so the extraction boundary stays reviewable:

```bash
git -C <project-root> add src/project.rs src/server.rs assets/plugin_src/00_main.lua assets/VertigoSyncPlugin.lua docs/configuration.md docs/quickstart.md docs/troubleshooting.md docs/superpowers/specs/2026-03-20-edit-preview-extraction-design.md docs/superpowers/plans/2026-03-20-edit-preview-extraction.md
git -C <project-root> commit -m "feat: integrate edit preview orchestration into vertigo sync"

git -C <vertigo-repo> add default.project.json
git -C <vertigo-repo> rm studio-plugin/VertigoEditSync.lua
git -C <vertigo-repo> commit -m "chore: adopt vertigo sync edit preview contract"

git -C <arnis-roblox-repo> add roblox/default.project.json scripts/run_studio_harness.sh
git -C <arnis-roblox-repo> commit -m "chore: adopt vertigo sync edit preview contract"
```
