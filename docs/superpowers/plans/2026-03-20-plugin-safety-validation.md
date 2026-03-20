# Plugin Safety Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a production-hardened generated-plugin safety validator to `vertigo-sync` and wire it into validation and plugin installation flows.

**Architecture:** Keep validation generic and local to `vertigo-sync`. Use real Luau tooling where available, then layer `vertigo-sync`-specific top-level and function-risk analysis over the generated plugin source. Expose one structured report and fail closed in CLI and tests.

**Tech Stack:** Rust, existing `vertigo_sync::validate` module, local `luau-compile` / `luau-analyze` binaries, CLI integration in `src/main.rs`.

---

### Task 1: Add failing tests for plugin safety validation

**Files:**
- Modify: `src/validate.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write failing unit tests for plugin-safety report helpers**

Add tests in `src/validate.rs` for:
- parsing/counting top-level symbols from a Luau snippet
- flagging an intentionally register-heavy function
- accepting a small clean function

- [ ] **Step 2: Run the targeted tests to verify they fail**

Run: `cargo test --manifest-path Cargo.toml plugin_safety -- --nocapture`
Expected: FAIL because plugin-safety helpers do not exist yet.

- [ ] **Step 3: Write failing CLI-facing test coverage**

Add or extend a test in `src/main.rs` to assert the generated plugin passes the new validator and reports the plugin artifact path.

- [ ] **Step 4: Run the targeted CLI/plugin test to verify it fails**

Run: `cargo test --manifest-path Cargo.toml embedded_plugin -- --nocapture`
Expected: FAIL because the new validation path is not implemented yet.

### Task 2: Implement the plugin safety validator

**Files:**
- Modify: `src/validate.rs`

- [ ] **Step 1: Add report types**

Create structured types for:
- plugin safety report
- issue entries
- function risk findings

- [ ] **Step 2: Implement Luau tool probes**

Add helpers that run:
- `luau-compile`
- `luau-analyze`

against a plugin path and convert failures into report issues.

- [ ] **Step 3: Implement top-level symbol counting**

Add a generic scanner that counts top-level local/function declarations in the generated plugin file and enforces the existing symbol budget.

- [ ] **Step 4: Implement function-level risk analysis**

Add a conservative function scanner that computes:
- parameter count
- local binding count
- multi-binding count
- nested closure count
- line span

and converts that into a risk score with named findings.

- [ ] **Step 5: Run targeted tests to verify they pass**

Run: `cargo test --manifest-path Cargo.toml plugin_safety -- --nocapture`
Expected: PASS

### Task 3: Integrate plugin safety into CLI flows

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Wire plugin safety into `vsync validate`**

Extend the validate command so it runs plugin safety checks and prints/fails appropriately.

- [ ] **Step 2: Wire plugin safety into `vsync plugin-install`**

Reject installation if the generated plugin report is not clean.

- [ ] **Step 3: Update embedded-plugin tests**

Replace the shallow budget-only assertion with checks that the generated plugin passes the full safety validator.

- [ ] **Step 4: Run targeted CLI tests**

Run: `cargo test --manifest-path Cargo.toml embedded_plugin -- --nocapture`
Expected: PASS

### Task 4: Verify end-to-end and document behavior

**Files:**
- Modify: `README.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/configuration.md` (only if validate docs mention the new gate)

- [ ] **Step 1: Document the validator briefly for open-source users**

Describe that `vertigo-sync` now validates generated plugin safety before install and during `vsync validate`.

- [ ] **Step 2: Run the full relevant verification**

Run:
- `cargo test --manifest-path Cargo.toml`
- `cargo fmt --all --check`

Expected: PASS

- [ ] **Step 3: Install-check the plugin flow locally**

Run: `cargo run --manifest-path Cargo.toml -- plugin-install`
Expected: plugin installs only if the safety validator is clean.
