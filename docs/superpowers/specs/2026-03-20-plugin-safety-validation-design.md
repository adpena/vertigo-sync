# Plugin Safety Validation Design

## Goal

Add a production-hardened local validation gate in `vertigo-sync` that catches generated Studio plugin failures before developers install or run the plugin in Roblox Studio.

## Problem

The current local checks are not sufficient for the failures we have actually seen in Studio:

- `Out of local registers`
- oversized top-level symbol pressure
- generated plugin regressions that compile in generic Luau tooling but still fail in Studio

`luau-compile` and `luau-analyze` are useful, but they are not enough on their own. We need a `vertigo-sync`-native validator that understands the generated plugin artifact and rejects risky bundles early.

## Scope

This design covers validation of the generated Studio plugin source embedded in `vertigo-sync`.

It does not attempt to statically prove all Studio behavior. It focuses on the specific class of failures that have already hurt development:

- compile failures
- analyzer failures
- top-level symbol budget breaches
- function-level local/register pressure likely to trip Studio

## Design

### 1. First-class plugin safety validator

Add a dedicated validation entry point in `vertigo-sync` for the generated plugin artifact.

This validator should produce a structured report with:

- hard failures
- warnings
- the analyzed plugin path
- top-level symbol count
- the worst function-risk findings with line ranges and names

The validator is generic and open-source friendly. It should not depend on `arnis-roblox`, `vertigo`, or project-specific builder paths.

### 2. Layered checks

The validator should run these checks in order:

1. `luau-compile` on the generated plugin source
2. `luau-analyze` on the generated plugin source
3. top-level symbol budget check
4. function-level register-pressure heuristic

The first two use available local Luau tooling. The latter two remain `vertigo-sync` logic because Studio-specific limits are not fully exposed by those tools.

### 3. Function-level register-pressure heuristic

The heuristic should be conservative and intentionally opinionated.

Each function gets a risk score derived from:

- parameter count
- `local` binding count
- multi-name `local` declarations
- nested closure count
- line span
- obvious giant-table/literal accumulation patterns if cheaply detectable

The validator should rank the worst functions and fail if any score crosses a hard threshold.

The output should tell developers which functions to fix first instead of just saying the plugin is “too large”.

### 4. CLI integration

The validator should be wired into:

- `vsync validate`
- `vsync plugin-install`
- generated-plugin tests in Rust

Behavior:

- `vsync validate` should report plugin safety alongside normal source validation
- `vsync plugin-install` should fail closed if the generated plugin is unsafe
- tests should protect against budget regressions in CI and local development

### 5. Future Studio smoke hook

The validator should be designed so a future empty-Studio smoke test can consume the same report format, but that smoke test is a second line of defense, not a replacement for static validation.

## Non-goals

- full semantic execution of plugin code outside Studio
- project-specific behavior validation
- guaranteeing absence of all Studio runtime bugs

## Success criteria

- unsafe plugin artifacts are rejected locally before install
- failure output identifies the concrete risky functions
- the validator remains generic and useful for open-source users of `vertigo-sync`
- the feature is documented as part of the normal `vertigo-sync` validation flow
