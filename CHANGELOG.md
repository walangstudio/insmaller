# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- `release.yml`: `actions/upload-artifact` v5 → v7. v5 still runs on Node 20
  (0.2.1's note was inaccurate — only `upload-artifact` v6+ is
  `runs.using: node24`; `checkout@v5`/`setup-python@v6`/`download-artifact@v5`
  were already Node 24). Silences the last Node 20 deprecation warning in the
  release workflow. Workflow-only; no crate change, no version bump.

## [0.3.0] - 2026-05-18

The leftover workspace-migration primitives deferred from 0.2.0. All new
schema is optional with serde defaults; existing catalogs/configs and the
default sentinel location are unaffected.

### Added
- **`merge_toml` / `merge_yaml` processors** — same contract as `merge_json`
  (a `command` emits a JSON patch deep-merged into `target`), for TOML and
  YAML config files. Existing target parsed strictly (an unparseable file is
  refused, not silently discarded), `--dry-run` writes nothing, output written
  atomically. `merge_json` is unchanged.
- **`backup` processor** — standalone composable step: a timestamped copy of
  `path` to `<dir>/<file>.<UTC>.<suffix>` before something mutates it. Missing
  path ⇒ skipped; dry-run ⇒ no copy.
- **`write_env` / `setup_output` array→CSV** — a JSON-array var (multiselect
  wizard field) now serializes as a comma-joined list (`KEY=a,b,c`) instead of
  being silently dropped; non-scalar elements skipped, empty array ⇒ bare
  `KEY=` (key kept), commas never force quoting (consumer splits on them).
  Restores parity with CSV `in` conditions.
- **`insmaller status`** (alias `query`) — read-only listing of recorded
  installs as an aligned table or `--json` array
  (`kind,key,version,spec,installed_at,post_done`); optional single-key filter.
- **`[settings] sentinel_scope` + `sentinel_path`** — `global` (default,
  unchanged) | `workspace` (anchored to the config's directory); an explicit
  `sentinel_path` overrides both. `Sentinel::resolve` + `Sentinel::base`
  added to the library API.

### Changed
- `atomic_write` is now `pub(crate)` (shared by the new merge processors);
  `merge_json`'s plain write is untouched.
- CLI sentinel construction routed through `Sentinel::resolve`; behavior is
  byte-identical to before at the default `global` scope.
- `serde_yaml` added as a workspace dependency.

### Security
- Bumped `ratatui` 0.29 → 0.30 (and `crossterm` 0.28 → 0.29) so the
  transitive `lru` resolves to 0.16.4, fixing RUSTSEC-2026-0002 /
  GHSA-rhfx-m35p-ff5j (`lru::IterMut` Stacked-Borrows UB). The vulnerable
  `iter_mut` path was never reachable here (ratatui uses `lru` only for its
  internal layout cache; insmaller never touches it), so this is hygiene, not
  an exploitable fix. ratatui pinned to `default-features = false` +
  `crossterm_0_29, underline-color, all-widgets, macros, layout-cache` to keep
  the termion/termwiz backends out. No source changes — 0.30 re-exports the
  used API unchanged.

### Notes
- `cargo test --workspace` is 224 tests, clippy clean; offline build verified
  (`serde_yaml` was already in the cargo cache; the ratatui 0.30 tree was
  fetched online once, then builds offline).

## [0.2.1] - 2026-05-18

### Changed
- CI/release workflows pin Node24-native action majors (`checkout@v5`,
  `setup-python@v6`, `upload-artifact@v5`, `download-artifact@v5`) and drop the
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24` opt-in env. No build behavior change;
  removes the Node 20 deprecation notices.

## [0.2.0] - 2026-05-18

Generic, reusable engine primitives so a downstream config-only consumer
(codetainyrrr) drives everything through `insmaller` + config. All new schema
is optional with serde defaults; existing catalogs and demos are unaffected.
The single `eval_condition` grammar stays the only expression evaluator.

### Added
- **Entry `condition`** — an entry/option is offered/installed only when its
  predicate holds; a conditioned-out entry is skipped (reported, counted
  completed), not failed. Honored on the direct `install_many` path too.
- **`requires_input` + `selected.inputs`** — an entry declares the inputs it
  needs; a wizard field `source = "selected.inputs"` expands in place into one
  field per declared input of the current selection (union, dedup by id,
  selection order), each gated by its own condition.
- **`[settings.setup_output]` + `write_env` processor** — emit the resolved
  vars to a single env file with an optional header and allowlist, written
  atomically (temp + rename, optional Unix mode).
- **Named tasks** — `[task.<name>]` ordered, per-OS, generic step pipelines
  with `needs` composition (cycle-guarded at load); `insmaller task <name>`
  (alias `insmaller run`). No Docker/container concepts in engine code.
- **`poll`** on shell/exec/check_command — `{ attempts, delay_ms,
  until_exit_zero }` wait-ready loop, distinct from on-error `retries`.
- **`[project]` block** — presentation strings (name/about, `intro_template`/
  `outro_template` rendered through the wizard vars) plus opaque pass-through
  `extra` available to task-script templating. Never read by install logic.
- **Configurable group order** — `project.group_order` orders wizard groups
  (unlisted after, alphabetical), then key within a group.
- **`provides_command`** — sugar that auto-appends a `check_command` verify
  step for an entry's binary.
- **Version-compare operators** in `eval_condition`: `>= <= > <` and
  semver-aware `== !=` (e.g. `${NODE} >= '20'`), with string fallback when a
  side is not version-like.
- Catalog-tree compatibility: `category` accepted as a serde alias of `group`;
  optional `name` label passthrough; unknown entry fields still ignored
  (no `deny_unknown_fields`).
- E2E fixtures under `examples/e2e-*` and integration coverage
  (`tests/codetainyrrr_e2e.rs`).

### Changed
- Windows `symlink` of a directory now tries a real symlink, then a directory
  **junction** (no privilege required), then a recursive copy — previously it
  fell straight to a copy.
- `install_many_with` takes a `run_vars` argument (entry-condition evaluation);
  `install_many` is unchanged and passes none.
- `WizardSession::fields()` returns owned `Vec<Field>` (synthetic
  `selected.inputs` fields are not part of `WizardDef`).
- `run_wizard` / `WizardSession::new` take a `group_order` argument.
- Step-pipeline execution exposed as `run_step_pipeline` for the task runner.

### Notes
- `release.yml` was already clean (no merge-conflict marker); the pipeline
  produces the four target archives + `SHA256SUMS`.
- `cargo test --workspace` is 203 tests, clippy clean.

## [0.1.0]

Initial release: config-driven installer engine — declarative step pipelines,
built-in processors, desugar table, recipe packs, optional wizard, sentinels,
JSON `EntrySource`, CLI (`install`/`uninstall`/`setup`), CI + release
workflows.
