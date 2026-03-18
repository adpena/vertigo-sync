<!-- SPDX-License-Identifier: LicenseRef-VERTIGO-CSL-1.0 -->
# Contributing to VERTIGO

Thanks for contributing.

Please follow `CODE_OF_CONDUCT.md` in all project spaces.

## Development Prerequisites

- Roblox Studio (latest)
- [`aftman`](https://github.com/LPGhatguy/aftman) for tool pinning
- Tools from `aftman.toml` (`rojo`, `wally`, `stylua`, `selene`)

Install project tools and packages:

```bash
aftman install
wally install
```

## Local Workflow

1. Start hotload:

```bash
./scripts/dev/rojo-hotload.sh
```

2. Configure plugin defaults once:

```bash
./scripts/dev/rojo-service.sh configure-plugin
```

3. In Studio Edit mode, connect Rojo to `127.0.0.1:35123`.

4. Save Lua files and verify they sync immediately in Explorer.

## Code Quality

Run formatting/linting before opening a PR:

```bash
stylua --check src
selene src
./scripts/dev/oss-readiness-doctor.sh --strict
```

If you add new scripts under `scripts/dev`, verify shell syntax:

```bash
bash -n scripts/dev/*.sh
```

## Design Alignment

Before implementing major changes, read:

- `vertigo_design_pack/docs/vision/05_MVP_Guardrails.md`
- `vertigo_design_pack/docs/specs/10_Gameplay_Loops.md`
- `vertigo_design_pack/docs/specs/17_UI_UX.md`

## Pull Request Expectations

- Keep changes scoped and focused.
- Include a short test plan in the PR description.
- Include before/after screenshots or videos for UI/feel changes.
- Do not commit secrets, API keys, or personal account IDs.
- Respect licensing boundaries in the root `LICENSE` (commercial rights are reserved).
- Check `LICENSES.md` before adding files to new directories.
- Do not move core gameplay logic into permissively licensed directories (`scripts/dev`, `.github`) unless explicitly approved.

## Reporting Bugs

When filing issues, include:

- OS and Roblox Studio version
- Rojo version (`rojo --version`)
- Repro steps
- Expected vs actual behavior
- Output from `./scripts/dev/rojo-service.sh doctor`
