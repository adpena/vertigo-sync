# Edit Preview Extraction Design

**Goal:** Move all generic edit-preview orchestration out of `vertigo` and into `vertigo-sync`, then update both `vertigo` and `arnis-roblox` to consume the shared `vertigo-sync` project contract.

**Scope:** `vertigo-sync` is the only home for reusable Studio sync/editor integration logic. `vertigo` and `arnis-roblox` keep only project-specific preview builders and project configuration.

## Problem

Today the edit-mode preview path is split across two plugins:

- `vertigo-sync` owns project parsing, file transport, builder roots, and Studio sync.
- `vertigo/studio-plugin/VertigoEditSync.lua` owns generic edit-preview rebuild orchestration.

That split is incorrect. The result is architectural drift:

- `VertigoEditSync` hardcodes `Server.Studio.StudioPreviewBuilder`.
- `vertigo-sync` only knows about low-level builder roots and not the higher-level edit-preview contract.
- Projects like `arnis-roblox` can be synced by `vertigo-sync` but still get noisy failures from the orphaned edit-preview plugin because the preview-builder convention was never pulled into `vertigo-sync`.

## Boundary

`vertigo-sync` must own all generic Studio integration logic:

- project config parsing
- Studio plugin runtime
- connection/project bootstrap
- builder execution
- edit-preview orchestration
- debounce/retry/backoff
- edit-preview workspace status attributes
- harness-facing plugin signals

Project repos keep only project-specific preview logic:

- preview builder module implementations
- project-specific world generation behavior
- project config that points at those builders

## New Project Contract

Add a new optional `vertigoSync.editPreview` config block to `default.project.json`.

Example shape:

```json
{
  "vertigoSync": {
    "builders": {
      "roots": ["src/ServerScriptService/StudioPreview/AustinPreviewBuilder.lua"],
      "dependencyRoots": [
        "src/ServerScriptService/StudioPreview",
        "src/ServerScriptService/ImportService",
        "src/ReplicatedStorage/Shared",
        "src/ServerStorage"
      ]
    },
    "editPreview": {
      "enabled": true,
      "builderModulePath": "ServerScriptService.StudioPreview.AustinPreviewBuilder",
      "builderMethod": "Build",
      "watchRoots": [
        "ServerScriptService.StudioPreview",
        "ServerScriptService.ImportService",
        "ReplicatedStorage.Shared",
        "ServerStorage"
      ],
      "debounceSeconds": 0.25,
      "rootRefreshSeconds": 1.0,
      "mode": "edit_only"
    }
  }
}
```

### Semantics

- `enabled`
  - Optional boolean.
  - Default: `false`.
- `builderModulePath`
  - Required when `enabled = true`.
  - DataModel path string to the preview builder module.
- `builderMethod`
  - Optional.
  - Default: `"Build"`.
  - Supports `"Build"` and `"BuildNow"` to bridge existing projects.
- `watchRoots`
  - Optional list of DataModel root paths used for edit-preview rebuild triggers.
  - If omitted, the plugin derives trigger behavior from `vertigoSync.builders.dependencyRoots` and mapped instances.
- `debounceSeconds`
  - Optional numeric override.
  - Default remains plugin default.
- `rootRefreshSeconds`
  - Optional numeric override.
  - Default remains plugin default.
- `mode`
  - Optional string.
  - Initial supported value: `"edit_only"`.

## Plugin Behavior

`VertigoSyncPlugin` gains a generalized edit-preview subsystem.

Responsibilities:

- resolve edit-preview config from `/project`
- validate preview config once on bootstrap
- schedule preview rebuilds only in Studio edit mode
- support both `Build()` and `BuildNow()` preview-builder entrypoints
- emit clear workspace attrs and logs for preview status
- debounce rapid source changes
- back off on repeated failures without spamming infinite noisy retries

The standalone `VertigoEditSync` plugin becomes obsolete.

## Failure Behavior

Fail closed and clearly:

- If `editPreview.enabled = true` but `builderModulePath` is missing or invalid:
  - set preview state to error
  - emit one structured error and bounded retries
  - do not silently fall back to some hardcoded path
- If `builderMethod` is missing on the resolved module:
  - emit explicit contract error
- If config is absent:
  - edit-preview orchestration stays disabled

## Compatibility

Backwards compatibility rules:

- projects without `vertigoSync.editPreview` keep current sync behavior unchanged
- existing `vertigo-sync` builder execution remains intact
- `vertigo` can adopt the new config with `builderMethod = "BuildNow"`
- `arnis-roblox` can adopt the new config with `builderMethod = "Build"`

The old `vertigo/studio-plugin/VertigoEditSync.lua` should be removed after the shared plugin path is verified.

## Repo Changes

### `vertigo-sync`

- add `editPreview` config parsing in `src/project.rs`
- include `editPreview` in `/project` payload
- add generalized edit-preview subsystem to `assets/plugin_src/00_main.lua`
- regenerate `assets/VertigoSyncPlugin.lua`
- document the new config in `docs/configuration.md`, `docs/quickstart.md`, and `docs/troubleshooting.md`

### `vertigo`

- remove `studio-plugin/VertigoEditSync.lua`
- add `vertigoSync.editPreview` to `default.project.json`
- keep `src/Server/Studio/StudioPreviewBuilder.luau` as the project-specific builder implementation

### `arnis-roblox`

- add `vertigoSync.editPreview` to `roblox/default.project.json`
- point it at `ServerScriptService.StudioPreview.AustinPreviewBuilder`
- update harness assumptions to trust `VertigoSync` preview signals instead of the removed standalone plugin

## Verification

Minimum verification bar:

- `vertigo-sync`
  - project parser tests for `editPreview`
  - plugin artifact contains new edit-preview config and runtime logic
- `vertigo`
  - no remaining dependency on `VertigoEditSync`
- `arnis-roblox`
  - Studio edit harness reaches connected `VertigoSync` state
  - preview builder is invoked through `VertigoSync`
  - no `missing ... StudioPreviewBuilder` noise from the old standalone plugin

## Non-Goals

- rewriting project-specific preview builders
- changing `arnis-roblox` fidelity logic in this extraction pass
- changing play-mode runtime generation behavior
