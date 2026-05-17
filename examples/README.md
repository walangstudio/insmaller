# examples/

Self-contained samples + an out-of-project end-to-end. Safe: no network, no
builds; the e2e writes only to a temp dir.

| File | Purpose |
|---|---|
| `run-demo.ps1` | The e2e. Copies the release binary + demo configs into a fresh `%TEMP%\insmaller-e2e-*` and runs dry-run / real install / idempotent re-run / terse spec / unattended wizard / theme env override with assertions. No TTY → no UI drawn. |
| `run-demo-ui.ps1` | Interactive counterpart: launches the **real ratatui setup TUI** against the demo configs so you can see and drive the wizard. Needs a real terminal. `-Theme`/`-NoColor` switches. |
| `demo.installer.toml` | Minimal engine config — `[settings]` (incl. `theme = "high-contrast"`) + a `hello:` desugar→recipe. No plugin packs. |
| `demo.catalog.json` | `demo` = inline generic steps (`prompt → shell → ensure_line → copy → sentinel_meta`, all under `DEMO_DIR`); `hello` = terse spec. |
| `demo.wizard.toml` | 2-page wizard (catalog multiselect + text). TTY → ratatui progress TUI; unattended → StaticAnswerer. |
| `siblings.catalog.json` | Real catalog for the sibling apps (mememo/chatgipite/gd-skills/borch/pair-pressure) via the generic inline-steps path. Also a fixture for `tests/e2e_scenarios.rs`. |
| `sample.wizard.toml` | Sample wizard over the sibling catalog. Also a fixture for `tests/wizard_e2e.rs`. |

## Run the e2e (in a temp folder, outside the project)

```
cargo build --offline --release -p insmaller-cli
pwsh -File examples\run-demo.ps1
```

Prints `ALL DEMO CHECKS PASSED` and leaves the temp dir for inspection.

## Try the interactive TUI

Copy the exe + the three `demo.*` files anywhere, then (just `--config` —
`demo.installer.toml` declares its catalog + wizard in `[settings]`):

```
set DEMO_DIR=C:/tmp/demo-out
insmaller install demo --config demo.installer.toml
insmaller setup        --config demo.installer.toml
```

`--catalog`/`--wizard` are still accepted and override the config. Rename the
file to `insmaller.toml` (or `.insmaller.toml`/`installer.toml`) and you need
**no flags at all** — it's auto-discovered in the cwd or any parent directory
(like `.env`/`Cargo.toml`), and its `[settings]` pulls in catalog + wizard:

```
insmaller install demo     # just works from anywhere in the project tree
insmaller setup
```

Or just run the interactive launcher (sets up a temp workspace and starts
the real TUI for you):

```
pwsh -File examples\run-demo-ui.ps1
pwsh -File examples\run-demo-ui.ps1 -Theme mono     # or default|high-contrast
pwsh -File examples\run-demo-ui.ps1 -NoColor
```

TUI keys: Tab/←→ focus · ↑↓ move · Space toggle · Enter Next · Esc Back ·
q/Ctrl-C quit. Header shows a progress gauge + `step N/M`; the install phase
shows an indicatif spinner. The TUI needs a real terminal — under a pipe or
CI it falls back to the non-blocking unattended path and draws nothing.

### Theming

`demo.installer.toml` sets `[settings] theme = "high-contrast"`. Presets:
`default` | `mono` | `high-contrast`. Override per run without editing config:

```
set INSMALLER_THEME=mono
insmaller setup --config demo.installer.toml
```

`NO_COLOR=1` (any value) forces `mono` and wins over everything. For exact
colors, uncomment `colors = { accent = "#..", ... }` in `demo.installer.toml`
(`#rrggbb`; invalid values keep the preset and warn).

## How users set up insmaller

Distribution is a **single ~6.8 MB release binary** (`insmaller.exe`) plus
plain-text config — no runtime/installer/deps:

1. Drop `insmaller` (the binary) on PATH.
2. Provide the config (or ship it alongside the binary):
   - `insmaller.toml` — engine config (`[settings]`, recipes, desugar,
     optional `[[plugin]]` packs). Auto-found in the cwd or any parent
     (`.insmaller.toml`/`installer.toml` also accepted), so no `--config`.
   - a catalog `.json` — what's installable (terse `install` spec **or**
     inline `steps`). Point `[settings] catalog` at it (or `--catalog`).
   - optional `wizard.toml` — the pages/selection UI; `[settings] wizard`
     (or `--wizard`).
3. Users run either:
   - `insmaller setup` — interactive ratatui wizard (pages, Back/Next,
     progress) → installs the selection; or
   - `insmaller <key…>` — direct install (insmaller is an installer, so a
     bare key with no subcommand defaults to `install`);
   - `insmaller uninstall <key…>` — runs each recipe's `uninstall` phase
     and clears its sentinels.

   `--answers F` / no-TTY = fully unattended (never blocks); `--dry-run`
   previews any of the above.

Inputs (API keys, paths) resolve from the environment via the non-blocking
`prompt`/`save_input` processors, so the unattended path is safe in CI.
