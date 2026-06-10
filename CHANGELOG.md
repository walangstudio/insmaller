# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.14.0] - 2026-06-10

### Added
- **Per-entry catalog `uninstall`.** A catalog entry may declare an `uninstall`
  shell command, run by `uninstall <key>` in addition to any recipe uninstall.
  This covers entries installed via a bare `install = "curl … | bash"` (the
  `shell-pipe` recipe, which has no uninstall) — they previously cleared their
  sentinel without removing the installed binary. The command runs as a real
  `shell` step (so it gets the full step machinery) before the sentinel is
  cleared, so a failed uninstall leaves the sentinel for a later retry.

## [0.13.0] - 2026-06-10

### Added
- **`files:` wizard source.** A field `source = "files:<dir>/<pattern>"` (single
  `*`, e.g. `files:~/.app/containers/*.env`) lists matching files' stems as
  choices. The dir part expands `~` and `${VAR}`; a missing or empty dir yields
  an empty list (no panic). Drives a "pick an existing item" page from the
  filesystem.
- **`[settings] defaults_from_file`.** Path (with `${VAR}` placeholders resolved
  from prior wizard answers) to an env-format file whose `KEY=value` entries
  prefill matching wizard fields as their effective default — for "edit
  existing" flows. Loaded lazily once the path's vars resolve; a missing file is
  a no-op. Secret fields with a saved value present as `__KEEP__`; the original
  is substituted back before writing and never displayed.
- **Positional task argument.** `insmaller task run <arg>` exposes a trailing
  non-task token to shell steps as `$CT_ARG` (and `{{ CT_ARG }}`). A non-task
  token is treated as the argument only when it is the last token and every
  earlier token is a known task; otherwise it stays a (failing) task name, so a
  typo is never silently swallowed.
- **`setup_output.path` templating.** The configured path is rendered with the
  resolved wizard vars (`${VAR}` or `{{ VAR }}`) before home-expansion, so a
  per-item path like `~/.app/containers/{{ CONTAINER_NAME }}.env` works. An
  undefined variable is an error, never a silent wrong-path write.

## [0.12.0] - 2026-06-09

### Added
- **Wizard review page.** A `[[page]]` with `review = true` (and no fields)
  renders a read-only summary of every collected answer (secrets masked) as the
  final step; Enter confirms and finishes, ← / Esc goes back to edit. New
  `WizardSession::summary_rows()` builds the labelled, masked rows.
- **Truecolor about-block filters.** `version_template` now supports
  `{{ x | rgb("818cf8") }}` (24-bit solid color) and a per-character
  `{{ x | gradient("818cf8","f472b6") }}`. `gradient` sets a color per char and
  resets once at the end, so chaining `| bold` keeps bold across the whole run.
  Both pass their text through unchanged when stdout isn't a TTY, `NO_COLOR` is
  set, or the hex is malformed.

### Fixed
- **Wizard skips a page left empty by `selected.inputs`.** A page whose only
  field is `source = "selected.inputs"` rendered blank when the selected
  entries declared no inputs (e.g. an API-keys page for a CLI that needs no
  key). Such a page — one that declared fields but has none after gating /
  expansion — is now skipped during forward/back navigation and progress
  counts. Info-only pages (no declared fields) still show.

## [0.10.0] - 2026-06-09

### Added
- **App version + `--version` about block, decoupled from the engine version.**
  `[project] version` sets the app's own version; `--version` then prints
  `<name> <version> (insmaller <engine_version>)` instead of the bare engine
  version. The whole block is overridable with `[project] version_template`
  (minijinja) — vars: `name`, `version`, `engine_version`, `about`, `copyright`,
  and every `[project.extra]` key under `extra.<key>`. Style filters (`bold`,
  `dim`, `italic`, `underline`, and the eight named colors + `gray`) emit ANSI
  when stdout is a TTY and `NO_COLOR` is unset, else pass through; filters stack
  (`{{ name | bold | cyan }}`). Emoji is just UTF-8 in the template. A broken
  template falls back to `<name> <engine_version>`, so `--version` never fails.

### Internal
- New `insmaller_core::about` module (`probe_project_meta` + `render_about`);
  `--version` best-effort reads only `[project]` from the discovered config
  (tolerant parse — a malformed-but-present config can't break `--version`).

## [0.9.0] - 2026-06-09

### Added
- **`[settings] setup_then_task`.** After `setup` finishes (including the
  `setup_writes_config_only` path), the engine can end with a default-yes
  `Run <product> now? [Y/n]` prompt and, on yes, run the named `[task.*]`
  in-process with the wizard's collected answers as task vars. The prompt text
  is overridable via `[settings] setup_then_task_prompt` (`{product}` expands to
  `project.name`). Skipped on non-TTY / `--answers` runs unless the new `--run`
  flag forces it; the new `--no-run` flag always skips. A `setup_then_task` that
  names a task absent from `[task.*]` is rejected at config load.

### Internal
- `cmd_setup` runs the hook at every success exit (no-packages, config-only, and
  both install paths), gating the install paths on a clean summary so a failed
  install never auto-runs the follow-up task.

## [0.8.0] - 2026-06-03

### Added
- **Cross-field assert validation.** A `[[page.field]]` entry can now carry
  `assert` and `assert_error` keys. `assert` is a comparison expression of the
  form `${VAR} OP ${VAR}` (or a literal on either side) where `OP` is one of
  `>= <= == != > <`. Both `${VAR}` operands are resolved from the accumulated
  vars map — meaning they can reference fields on the *current* page or on any
  prior page. The comparison is date/datetime-aware (ISO `YYYY-MM-DD` and
  `YYYY-MM-DDTHH:MM:SS`), then numeric, then version-string-aware, then
  lexicographic string. Example:
  ```toml
  [[page.field]]
  id = "go_live_end"
  type = "date"
  required = false
  assert = "${go_live_end} >= ${go_live_date}"
  assert_error = "End date must be on or after the go-live date."
  ```
  When `assert` fails, the field stays focused and the `assert_error` message
  (or a default "does not satisfy: …" message) is shown in the footer.
- **Assert gate in the interactive TUI.** In the `KeyCode::Enter` submit path,
  cross-field asserts run after partial-date, path, and API validation. The gate
  builds candidate vars from prior-page answers plus the current page's
  just-committed values, so forward and backward references both resolve.
  Optional fields with no value skip their own assert (consistent with how
  per-field validators skip empty optional values).
- **Assert enforcement in `--answers` / unattended runs.** `run_wizard` already
  evaluates asserts as a final pass, so headless runs enforce the same rules as
  the interactive TUI.
- **Schema validation for assert.** `validate_wizard_schema` rejects a malformed
  `assert` expression (no recognised operator), an empty `assert`, and
  `assert_error` without a matching `assert`.
- **Demo.** `examples/wizard-widgets.toml` now includes a `go_live_end` field on
  the Schedule page that demonstrates the cross-field assert against `go_live_date`.

### Changed
- An `assert` is skipped whenever any referenced `${VAR}` is blank or absent, so
  optional fields and forward references never fail spuriously — enforced
  identically in the TUI and the `--answers` path.

### Internal
- The release workflow now publishes both crates to crates.io (`insmaller-core`
  then `insmaller`, with an index-wait retry) after the platform builds succeed.
  It no-ops when `CARGO_REGISTRY_TOKEN` is not set.

## [0.7.0] - 2026-06-01

### Added
- **`dropdown` field type.** A collapsed, type-to-search select: the list is
  hidden until the user starts typing, then filters to matching options. Distinct
  from `single_select`, which remains an expanded radio list. The popup shows a
  `Search:` line at the top; typing narrows the list in real time; `[no matches]`
  is shown when nothing matches.
- **`textarea` field type.** Multi-line free text; newlines are preserved in the
  collected value. Press **Enter** to enter edit mode (`[Enter to edit]` shown
  when focused but idle); **Esc** exits edit mode. While editing, ↑↓←→/Home/End/
  PgUp/PgDn navigate within the text, Enter inserts a newline, and a
  `(line N/total)` counter is shown. While not editing the field participates in
  normal Tab/arrow field navigation.
- **`date` field type.** Digit-only masked entry; the `-` separators at
  positions 4 and 7 of `YYYY-MM-DD` are pre-placed and auto-skipped. Empty digit
  slots are shown as `_`. Range bounds via `min`/`max` (ISO date strings). Press
  **Space** to open a month calendar overlay (←/→ = ±1 day, ↑/↓ = ±1 week,
  PgUp/PgDn = ±1 month, Enter commits, Esc cancels). On page re-entry the cursor
  resumes at the first unfilled slot. A partially-typed date (at least one digit
  filled but not all) is rejected with an "incomplete date" error instead of
  being silently submitted empty.
- **`datetime` field type.** Same masked-entry and calendar mechanics as `date`,
  with 14 digit slots for `YYYY-MM-DDTHH:MM:SS`. The calendar commits only the
  date portion; any time digits already typed are preserved.
- **`[page.field.api]` — field-level API validation.** After local validators
  pass, the engine renders `{{value}}` into the configured `url` (and optionally
  `headers`/`body`), fires an HTTP request, and accepts the value only if the
  response matches `expect_status` (default: any 2xx) and, when set,
  `expect_json_path` resolves truthy. On failure the field stays focused and
  shows the `error` message. Keys: `url` (required, http/https, may contain
  `{{value}}`), `method` (default `GET`), `headers` (array of `[name, value]`
  pairs), `body`, `expect_status`, `expect_json_path`, `timeout_ms` (default
  5000), `error`. Skipped on `--answers` / unattended runs by design;
  `--no-api-validate` skips all API checks (useful offline/CI).
- **Path field validation.** A typed path value is trimmed of surrounding
  whitespace and rejected if neither the path itself nor its parent directory
  exists (catches typos and wrong case on Linux). A new leaf under an existing
  parent is accepted; a bare name with no path component (implicit cwd parent) is
  always accepted.
- **Collected answers are printed after the wizard.** Secret fields are shown
  as `***`. When answers were collected but no packages are selected, setup prints
  `No packages to install (answers recorded above)` and exits cleanly.
- **Field navigation.** Tab/Shift-Tab move between fields and skip a disabled
  Back button; the focused field is always visibly highlighted with a `▶` marker
  and a border focus-glow (colored themes).
- **`examples/wizard-widgets.toml` + `examples/serve-validate.py`.** A bundled
  demo that exercises every new field type. `serve-validate.py` is a dependency-
  free Python HTTP server on `127.0.0.1:8787` that accepts keys starting with
  `demo-` — letting you exercise the full `[page.field.api]` path entirely on
  localhost without any third-party service or signup.
- **Schema validation of the wizard at load time.** `validate_wizard_schema` is
  called before the TUI starts and rejects: an empty or non-http(s) `api.url`;
  `format=` on non-text fields (select/date/datetime/toggle); a numeric `min`/
  `max` on a date/datetime field; a quoted/string `min`/`max` on a non-date
  field; and `source = "catalog.*"` on a dropdown (dropdown answers are not
  pushed to `selected_keys`).

### Changed
- `min`/`max` on a field now accept an ISO date string in addition to a number.
  Existing numeric bounds are unaffected (backwards compatible).
- Validation error messages use the field's `prompt` label instead of its `id`,
  matching the text the user sees on screen.

## [0.6.2] - 2026-05-31

### Added
- Per-crate `README.md` for `insmaller` and `insmaller-core` so the crates.io
  pages document installation and usage. (No code changes; version bumped only
  because crates.io renders the README from the published version.)

## [0.6.1] - 2026-05-31

### Fixed
- **Drive selector no longer yields an empty path.** Pressing `s` (or selecting
  `.`) while on the Windows drive list returned the empty sentinel path as the
  field value; `select_cwd` now returns `None` there and `s` is a no-op (a
  single `at_drive_selector()` predicate owns the sentinel so it can't leak).
- **Drive enumeration can't hang on dead network mappings.** `windows_drives`
  now reads the `GetLogicalDrives` bitmask instead of stat-probing `A:`..`Z:`
  with `is_dir()`, which could block for seconds on a disconnected mapped drive.
- Docs (README, examples) list `modern` as the default theme; the old preset
  list omitted it.

### Internal
- Header gradient is cached by width (rebuilt only on resize, not every frame).
- `ascend` at a root now delegates to `goto_drives` (removes duplicated branch).

## [0.6.0] - 2026-05-30

### Added
- **Cross-drive file browser (Windows).** The `Ctrl+B` path browser can now
  leave the current drive: at a drive root (`C:\`) pressing `←` ascends to a
  drive selector that lists every available drive (`C:`/`D:`/…); selecting one
  descends into it. The `..`/ascend mechanism was generalized rather than
  special-cased — Unix (single `/` root) is unchanged.
- **Modern default theme (`modern`).** A midnight indigo→violet truecolor
  palette is now the default look: rounded panel borders, a focus-glow border
  on the active panel, a gradient progress header that animates a subtle sheen
  on interactive terminals, and a drop shadow behind the file-browser modal.
  Animation auto-disables under `NO_COLOR`/`mono`/non-TTY (no idle wakeups).
- **More theme roles.** `[settings.colors]` accepts `accent2`, `border`,
  `border_focus`, `shadow`, and `success` hex overrides in addition to the
  existing roles.

### Changed
- The legacy flat-cyan look is still available via `theme = "default"` (or
  `INSMALLER_THEME=default`); `high-contrast` and `mono` are unchanged.

## [0.5.3] - 2026-05-29

### Fixed
- **`confirm` no longer aborts value-less steps.** The gate now only applies
  to steps that produce a scalar value (`prompt`/`input`/`save_input`); on a
  step with no value (`shell`/`exec`/`copy`/…) it's a no-op instead of an
  unconditional abort. Comparison uses the engine's `scalar_str`, so a
  Bool/Number value compares as `true`/`42` (not its JSON form), matching how
  `write_env`/registers stringify the same value. The mismatch error now names
  the step (kind + `register_as`) for diagnosability — never the value.
- **No-subcommand dispatch reads settings with a typed parse.** `default_command`/
  `default_args` are now deserialized (type-checked, rename-tracking) instead
  of hand-walked from raw TOML — a non-string `default_args` element or a
  non-string `default_command` is now an error (warned + ignored) rather than
  silently dropped, matching the authoritative config load.
- **Masked secret prompt cancels on uppercase Ctrl+C/Ctrl+D too** (CapsLock /
  Ctrl+Shift), not just lowercase.

### Internal
- `scalar_str` is shared between the env writer and the `confirm` gate (one
  value→string rule). The `confirm` doc clarifies it's only meaningful on
  value-producing steps and that a mismatch is not retried.

## [0.5.2] - 2026-05-29

### Changed
- **`confirm` is now a generic step gate.** It moved out of the `prompt`
  processor into the orchestrator, so any value-producing step (`prompt`,
  `input`, `save_input`, an `exec` bound with `register_as`, …) can gate on
  its produced value: `confirm = "RESET"` aborts the step unless the value
  matches (rendered through ctx; empty/absent = no gate; a skipped optional
  step is a no-op). Behavior for existing `prompt`/`input` steps is unchanged.
- The masked-secret line editor's per-key rules (Ctrl-chord dropping,
  Ctrl+D-as-cancel, backspace-on-empty) and the paste line-break filter are
  extracted into pure functions with unit tests — closing the "hand-rolled
  reader has no coverage" gap without swapping to a crate that would drop the
  `*` echo and the bracketed-paste multi-line protection.

### Performance
- The no-subcommand dispatch path (`default_command`) reads only the two
  settings it needs via a lightweight TOML peek instead of building a full
  `LoadedConfig` (recipe indexing, plugin merge, cross-ref) that the chosen
  `cmd_*` then rebuilds — one authoritative config load instead of two.

## [0.5.1] - 2026-05-29

### Fixed
- **Interactive prompts no longer block a tokio worker thread.** The blocking
  stdin/crossterm read (and the `INTERACTIVE_LOCK` wait) now run under
  `block_in_place` on the multi-thread runtime, so a `prompt`/`input` step
  doesn't starve step-timeout timers or other parallel tasks in the same wave.
- **Pasted secrets are no longer silently mutated.** A bracketed paste into a
  `secret = true` prompt now strips only newlines/carriage-returns (to collapse
  a multi-line paste); tabs and other bytes are kept verbatim so the captured
  value matches the source.
- **Setup-wizard install phase now prompts at the TTY.** Running `insmaller
  setup` interactively gives install-recipe `prompt` steps a TTY resolver
  (unless `interactive_tasks = false`), instead of failing fast env-only. The
  spinner is suppressed on that path so a masked prompt isn't garbled by
  repaints.
- **Registry alias resolution follows chains and never advertises a dead
  alias.** `get` walks alias→alias→canonical (cycle-bounded); `known` only
  lists aliases that resolve to a registered processor, so the advertised set
  equals the resolvable set.

### Internal
- `env_nonempty` helper in `insmaller-core` is the single definition of
  "env value present" (empty = absent), shared by `EnvResolver` and the CLI's
  interactive resolver.
- Hardened the bracketed-paste guard against accidental early-drop and
  collapsed a duplicated setup-install dispatch branch.

## [0.5.0] - 2026-05-28

### Added
- **Interactive `prompt`/`input` task steps.** A `prompt`/`input` step in a
  task may now read a value from the user on a TTY, including a masked secret
  (`secret = true`). A new `confirm = "X"` param gates the step on exact match
  (renders through ctx, so `confirm = "{{ project_name }}"` works; empty or
  missing means no gate). The new `input` kind is a forwarded alias for the
  existing `prompt` processor — a plugin that overrides `prompt` automatically
  takes effect for `input` too. Cancelling an optional prompt (Esc / Ctrl+C /
  Ctrl+D) returns Skip rather than aborting the task; required prompts still
  Fail. Other Ctrl+letter chords (Ctrl+U, Ctrl+W, …) are silently dropped from
  the secret buffer instead of pushed as literal control bytes. Bracketed
  paste is enabled during a `secret` read so a pasted multi-line payload is
  consumed as one value rather than leaked across prompts.
- **`[settings] interactive_tasks`** (tri-state) controls when prompts read
  stdin: `Some(true)` → on for every command (install/uninstall/setup/task);
  `Some(false)` → off everywhere; `None` (default) → on for `task`, off for
  install/uninstall/setup so the historical fail-fast contract for install
  recipes is preserved unless explicitly opted in. Non-TTY runs always fall
  back to env-only.
- **`[settings] default_args`** prepended to the user's argv when
  `default_command` fires. Together with the new dispatch logic this means
  `insmaller`, `insmaller --dry-run`, and `insmaller foo` all route through
  the configured default (with `default_args` ++ user args) instead of the
  install catch-all; an explicit subcommand (`insmaller install x`) still
  bypasses defaults. `--config` is honored at this dispatch layer too, so
  default-command lookup respects the user's explicit config flag.

### Changed
- Dispatch no longer treats an unknown first token as install when
  `default_command` is set — it goes through the configured default instead.
  Behavior with no `default_command` is unchanged (bare → usage+fail; unknown
  → install).
- A malformed `installer.toml` encountered during default-command lookup
  prints a stderr warning instead of being silently treated as 'no default'.

### Fixed
- **Windows legacy console double-keystroke** in the masked secret reader:
  `crossterm::KeyEvent.kind` is now filtered to `Press`/`Repeat`, so a
  Windows console that emits both Press and Release events no longer
  records every typed character twice.
- **Raw mode is now panic-safe**: the masked-input reader uses a Drop-based
  `RawModeGuard` instead of inline cleanup, mirroring `tui.rs::TermGuard`.
  A panic mid-read no longer leaves the terminal wedged in raw mode.
- **Parallel-task race**: a process-global mutex serializes interactive
  reads so two `[task].parallel = true` tasks each calling a `prompt` step
  no longer race `enable_raw_mode` / `event::read` / `disable_raw_mode`
  against each other.
- **Stdout-redirected prompts**: `is_tty()` now requires both stdin AND
  stdout to be terminals. `insmaller task t > log.txt` now defers to the
  env fallback instead of writing the prompt invisibly to the log file
  while the user types blind.
- **`confirm` template error on absent optional step**: `ctx.render` for
  `confirm` now happens inside the `Value(v)` arm, so a `confirm =
  "{{ x }}"` with `x` undefined no longer errors an optional prompt that
  would have skipped anyway.
- **BarReporter race with prompts**: when `interactive_tasks = true` is
  set, `cmd_setup`'s install phase uses the plain `StdoutReporter` instead
  of the indicatif spinner so prompt output isn't overwritten by spinner
  repaints.

## [0.4.0] - 2026-05-26

### Added
- **Wizard field validators** on fields and catalog `requires_input`: `pattern`
  (regex, anchored), `format` (`integer`/`number`/`alpha`/`alnum`/`email`),
  `min`/`max`, `min_length`/`max_length`, and a custom `error` message. Enforced
  interactively (re-ask) and on the unattended `--answers` path; NaN/inf
  rejected. See `docs/fields.md`.
- **Declarative task concurrency.** Per-task `parallel` opt-in (default
  exclusive); the `needs` DAG runs independent tasks concurrently, throttled by
  `[settings] max_parallel_tasks`; CLI `--jobs N` / `--parallel`.
- **Task gating** via `[task].when` / `unless` (a gated-off task is skipped and
  treated as satisfied so dependents still run).
- **`[settings] default_command`** — a bare `insmaller` invocation runs it (e.g.
  `setup`).
- **TUI:** arrow navigation between fields; a collapsible catalog group tree
  (default collapse via `start_groups_collapsed` / `collapsed_groups` /
  `expanded_groups`, persisted across pages); a `Ctrl+B` file/dir picker
  (folders selectable, not just files).
- **`[settings] setup_writes_config_only`** (collect config + write
  `setup_output`, run no host install) and **`prefer_bash_on_windows`** (run
  POSIX shell steps through Git Bash when present, detected against the enriched
  PATH).
- **Multi-instance safety:** a cross-process sentinel lock (acquired off the
  async executor) and per-process unique `atomic_write` temp names.
- **AV hardening:** embedded Windows version resource + `asInvoker` manifest,
  stripped/LTO release profile, an opt-in signing step, and `docs/antivirus.md`.

### Changed
- Removed consumer-specific branding from the engine; identifiers are generic.

## [0.3.3] - 2026-05-23

### Added
- **exe-sibling config discovery (S1).** `discover_config` gains a tier between
  cwd+ancestors and app-home: an `installer.toml` sitting next to the running
  binary (`dir(current_exe())/installer.toml`). Lets a freshly-extracted bundle
  run `./bundle/the reference installer task install` from any cwd and find its own recipe
  with no `--config` and no `cd`. Precedence: `--config` > cwd+ancestors >
  exe-sibling > app-home > legacy `installer.toml`. Only the legacy name is
  probed next to the binary, so a stray `insmaller.toml` in a shared bin dir
  can't hijack discovery. `current_exe()` failure degrades silently.
- **`self_exe` / `exe_dir` task vars (S2).** `insmaller task <name>` injects the
  running binary's path and its parent dir into `run_vars`, so a `[task.*]`
  recipe can `copy {{ self_exe }}` and `{{ exe_dir }}/payload/*` from any cwd.
  Injected with `or_insert` so a `project.extra`/env value of the same name
  wins; `current_exe()` failure (or a parentless path) injects nothing.

  Together these let a config-only consumer ship a self-installing bundle
  (binary + sibling `installer.toml` + `payload/`) driven entirely by an
  insmaller `[task.install]` recipe, with no bespoke install script. Pure
  mechanism — no consumer-specific names in engine code.

## [0.3.2] - 2026-05-22

### Added
- **argv0-derived program name + app-home config discovery (P4).** The CLI
  derives `<name>` from argv0 (`Path::file_stem`, so `.exe` is stripped;
  falls back to `"insmaller"`). A binary renamed to `the reference installer` now prints
  `usage: the reference installer …` and `the reference installer 0.3.2`, and `discover_config`
  gains an app-home fallback after the existing cwd+ancestors walk:
  - POSIX: `$XDG_CONFIG_HOME/<name>/installer.toml` (else
    `~/.config/<name>/…`), `~/.<name>/installer.toml`, `/etc/<name>/…`.
  - Windows: `%APPDATA%\<name>\installer.toml`,
    `%USERPROFILE%\.<name>\installer.toml`, `%PROGRAMDATA%\<name>\…`.

  Lets a rebranded engine installed under a per-user app-home dir be invoked
  from any cwd with no `--config` flag. cwd+ancestors discovery keeps
  precedence; `-c/--config` still overrides everything. Pure mechanism — no
  consumer-specific names in engine code.

  An empty-but-set `XDG_CONFIG_HOME`/`%APPDATA%`/`%PROGRAMDATA%` is treated as
  unset (per the XDG spec) so it falls back to the `dirs::*` location instead
  of producing a bogus relative candidate.

## [0.3.1] - 2026-05-19

### Changed
- `release.yml`: `actions/upload-artifact` v5 → v7. v5 still runs on Node 20
  (0.2.1's note was inaccurate — only `upload-artifact` v6+ is
  `runs.using: node24`; `checkout@v5`/`setup-python@v6`/`download-artifact@v5`
  were already Node 24). Silences the last Node 20 deprecation warning in the
  release workflow. No source/behavior change.

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
drives everything through `insmaller` + config. All new schema
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
  (`tests/host_fixture_e2e.rs`).

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
