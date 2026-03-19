# Contributing to Vertigo Sync

Thanks for contributing.

Please follow `CODE_OF_CONDUCT.md` in all project spaces.

## Development Prerequisites

- Rust (stable)
- Roblox Studio (latest)
- [`aftman`](https://github.com/LPGhatguy/aftman) for optional Roblox-side tooling
- Tools from `aftman.toml` when working on Roblox-side fixtures (`stylua`, `selene`)

Install local tools:

```bash
cargo build
aftman install
```

## Local Workflow

1. Start the sync server from this checkout:

```bash
cargo run -- serve
```

2. Install the Studio plugin:

```bash
cargo run -- plugin-install
```

3. Restart Studio and use the Vertigo Sync plugin.

4. Save source files and verify they sync into Explorer.

## Code Quality

Run checks before opening a PR:

```bash
cargo test --all-targets --all-features
```

If you touch Roblox-side Lua or shell scripts, also run:

```bash
stylua --check .
selene .
bash -n scripts/*.sh
```

## Pull Request Expectations

- Keep changes scoped and focused.
- Include a short test plan in the PR description.
- Include before/after screenshots or videos for UI/feel changes.
- Do not commit secrets, API keys, or personal account IDs.
- Respect the root [LICENSE](/Users/adpena/Projects/vertigo-sync/LICENSE).
- Keep generated plugin artifacts and their source in sync when a change affects plugin packaging.

## Reporting Bugs

When filing issues, include:

- OS and Roblox Studio version
- `cargo --version`
- Repro steps
- Expected vs actual behavior
- Relevant `vsync` logs or Studio Output lines
