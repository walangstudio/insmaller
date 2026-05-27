# insmaller — Iteration 2: processors, integrations, plugins/WASM, tri-platform

**STATUS: COMPLETE — P0–P5 done, 88 tests green offline, `--features cdylib`
builds. WASM documented-deferred (extism not in this box's offline cache; add
`extism` + `WasmTransport` per the sketch in `crates/insmaller-core/src/plugin.rs`
when building online — same JSON protocol, drop-in).**

Authoritative plan for the second build iteration. Mirrors the approved plan.

## Context

Engine MVP (B1–B5, 61 tests green) → expand into a general installer: more
common-installer coverage, useful integrations, **plugin + external-process + WASM/native
extensibility**, across **Linux + macOS + Windows**. Autonomous full P0–P5.

Locked decisions: extensions = recipe-pack (E2) + external-process (E1) + native/WASM
(E4); NO embedded scripting (E3). Engine principle: **installers are recipes (TOML),
processors only for new capabilities.** Windows inclusion forces platform-aware
shell/PATH (engine currently hardcodes `bash -c` + `:`-PATH).

## Invariants
- Parity oracle frozen: gh-release/go-toolchain embedded scripts + `script_file`
  resolution, 9 verbatim `ParseKind` parsers, `desugar.rs`/`e2e.rs` ports. New
  download/extract are for NEW recipes only.
- Additive only: `StepOutput: Default`==old `()`; new config `#[serde(default)]`;
  plugins can't shadow core.
- Strangler-compatible: `EntrySource`/`EntryRef` unchanged; `json_catalog.rs` compiles
  untouched.
- 61 tests green every P-checkpoint (`cargo test --offline`; no "install" in
  crate/bin names).

## Milestones

**P0 Foundations (highest risk, no behavior change):** widen `Processor::run` →
`anyhow::Result<StepOutput>` (`StepOutput: Default`, mechanical `Ok(default())` for 6
built-ins); `register_as` → run_steps-local map via `Ctx::render_with` (Ctx stays
immutable); Skip-propagation (prompt→Skip marks var absent, skips dependents, no
strict-undefined error); platform probe into `Ctx::new()` (`os`,`os_family`,
`pkg_manager`,`exe_ext`); platform-aware `pathenv` (`bash -c` unix / `powershell` win;
`:`/`;` PATH) keeping unix byte-identical.

**P1 Four native processors:** `prompt` (first real `InputResolver` consumer,
non-block), `save_input` (.env write + register), `download` (URL→file + integrated
sha256 + GITHUB_TOKEN, cross-platform), `extract` (tar/zip/gz/bz2/xz cross-platform).
Excluded as recipes/idioms: git, copy, symlink, chmod, ensure_line, template,
standalone checksum/verify_sig.

**P2 Recipe-pack plugins (E2) + breadth:** `[[plugin]] name= path=` →
`installer.plugin.toml` merged, recipes namespaced `name/<r>`, prefixes not; core wins
collisions, plugin↔plugin = hard load error. `ParseKind::VersionedPkg`
(`name[@version]`). Recipe packs: Linux apt/apk/dnf/pacman/zypper, macOS brew, Windows
winget/scoop/choco, lang pip/pipx/cargo/gem/dotnet-tool/rustup/asdf/mise/pnpm/yarn/
composer/deno/bun/go-install. Multi-OS via `when={{ pkg_manager }}`; Windows uses
exec/powershell.

**P3 External-process plugins (E1):** registry fallback → `ExternalProcessor` from
`[[plugin]] command= kinds=[..]`. Subprocess JSON stdin→stdout, exit=status. Req
`{protocol:1,kind,params(rendered),ctx,dry_run}` resp
`{protocol:1,ok,register,log[],message}`. Integer protocol versioning (refuse unknown).
`sandbox=true` → env allowlist + timeout + no secret passthrough unless `pass_env`.
Step attrs `timeout`/`retries`.

**P4 verify + dry-run + structured Reporter:** per-recipe `verify: Vec<Step>` + `verify`
phase; orchestrator `dry_run` (no-op+report, never fail on missing optional input); JSON
event Reporter (powers the reference installer catalog-smoke).

**P5 WASM + native dynamic (E4):** WASM via `extism` (sandboxed, ABI handled),
`[[plugin]] wasm= kinds=[..]`, `--features wasm`. Native cdylib via `libloading`,
`[[plugin]] cdylib= kinds=[..]`, `--features cdylib`, unsafe boundary documented. Both
reuse the E1 protocol → one protocol, three transports.

## Critical files
`crates/insmaller-core/src/`: processor.rs (trait widen), orchestrator.rs (register_as,
skip-prop, verify, dry-run), config.rs (`[[plugin]]`, VersionedPkg, conflicts),
processors.rs (4 new), pathenv.rs (platform shell/PATH), ctx.rs (platform probe),
registry.rs (plugin fallback), new plugin.rs (PluginTable + transports). installer.toml
+ plugins/*/installer.plugin.toml.

## Verification
`cargo test --offline` green per P (baseline 61, additive). P0 trait-widen
byte-identical; P1 prompt non-block + download sha256 + extract round-trip; P2 namespace
+ core-wins + plugin collision; P3 external echo + version-refuse + sandbox; P4 verify
fail + dry-run spawns-nothing + JSON shape; P5 extism sample (gated) + cdylib smoke.
Strangler gate: e2e.rs + desugar.rs + json_catalog unchanged & green.

## Validation findings (gap audit, 94 tests green)

E2e scenario suite (`tests/e2e_scenarios.rs`) exercises register-flow, the
prompt/Skip keystone, fail-fast non-block, the retry loop, dry-run through the
real plugin pipeline, and real `exec` — all green.

**ALL RESOLVED (98 tests green, cdylib builds warning-free):**
- **Bug:** skipped step reported `ok=false` ("FAILED") though the key
  succeeds → now reports ok + a "skipped" log. (scenario-tested)
1. **Uninstall path — DONE.** `uninstall_many` + `uninstall_one`: resolves
   `recipe.uninstall` via desugar, runs it, clears both sentinels; non-
   recursive (no dep cascade); not-installed = no-op. Re-exported, round-trip
   tested.
2. **timeout-kill — DONE.** `ProcessTransport` sets `kill_on_drop(true)` (the
   engine-timeout drop now kills the child); `download` self-bounds via a
   ureq global timeout (step `timeout`, default 600s) so it never relies on
   the un-killable `spawn_blocking`. (cdylib FFI remains inherently
   un-interruptible — documented.)
3. **command split — DONE.** Quote-aware `split_command` (groups `"…"`/`'…'`)
   replaces `split_whitespace`. Tested incl. spaced Windows paths.
4. **`shell_literal` guard — DONE.** Catch-all applies only when the spec is
   a genuine shell pipeline (`curl`/`wget`/`sh`/`bash`/`| sh`/`| bash`); a
   typo'd/unknown spec now errors `NoDesugar` instead of silently executing.
5. **post_install — resolved by design.** `run_sh` is platform-aware
   (bash↔powershell); command *content* is the recipe author's concern, not
   an engine gap. Noted.
6. **`from_str` plugins — DONE.** Retains `command`/`wasm`/`cdylib` plugins
   (only `path` needs a base dir → errors); `register_external` now works
   with a from_str config. Tested.
- Cleanup: removed the dead `wasm` cfg branches (extism dep was dropped) —
  no compiler warnings.

## Post-magent-audit hardening (A–E + switches) — DONE, 114 tests green

Four parallel magent reviewers (debugging/security/qa/architect) confirmed the
6 gaps + bug fixed & test-locked, then surfaced a deeper set. All actioned:
- **A merge_json** no longer hardcodes `bash` — routes through new
  `pathenv::run_capture` (bash↔powershell). Latent Windows crash closed.
- **B extract** `safe_join` rejects `..`/abs/prefix; tar symlink+hardlink
  entries refused (zip-slip / link-traversal closed).
- **C plugin `path=`** canonicalized + bound under the config dir (`../`
  escape rejected at load).
- **D download** bearer requires https + optional
  `auth_bearer_allowed_origins` allowlist; `require_sha256_for_exec` switch.
- **E architecture**: `Step.params` migrated `toml::Table → serde_json::Map`
  (TOML no longer leaks past parse; `render_params` simplified); dead
  `Lifecycle.phases` removed; **real cycle detection** (gray/black DFS) — a
  true cycle now errors with no sentinel, diamond deps still fine;
  `builtins(&Settings)` (no bare `path_globs` arg).
- **Switches + `SECURITY.md`**: `allow_shell_literal`,
  `auth_bearer_allowed_origins`, `require_sha256_for_exec` in `[settings]`;
  SECURITY.md documents the trusted-config model + accepted-by-design
  surfaces + the switches.
- Test backfill: retry-then-success, timeout-elapses, uninstall-unknown,
  verify-success, dry-run-skips-post_install, origin/mode helpers, bearer
  https/allowlist bail, sha256-for-exec bail, extract unsupported/total-strip/
  bz2/symlink-refused, safe_join, plugin-path-traversal, allow_shell_literal
  off. 98→**114 tests, 0 failures**; `--features cdylib` clean; CLI
  `--dry-run --json` smoke green.

## Re-audit #2 (post-hardening) — all findings actioned, 114 tests green

3 parallel magent reviewers re-audited the post-A–E code: debugging confirmed
**zero regressions** (all A–E VERIFIED); security/architect surfaced more, ALL
fixed (user chose "everything incl. refactors"):
- **Defects:** external-plugin `dry_run` now threaded via `Ctx.dry_run` (was
  hardcoded false); **ZIP symlink entries refused** (was tar-only); `origin_of`
  rejects userinfo + bearer requires plain scheme://host (token-exfil closed).
- **Smells:** `run_sh` now reuses `shell_invocation` (one dispatch);
  `EngineError` split into `StepFailed/DepFailed/PostInstall/Verify/Cycle/
  BadSpec/NotFound` (Display strings stable — string-asserting tests
  unchanged); **`EngineCtx` struct** collapses the private 6-arg cluster
  (public `install_many*`/`uninstall_many*` signatures kept = stable
  contract); all `#[allow(too_many_arguments)]` removed from private fns.
- **Hardening:** sentinel `kind`/`key` path-sanitized; `split_command` errors
  on unterminated quote; `Step::param_bool/i64/array` (killed `bool_param` +
  inline dups); minijinja `Environment` cached in `OnceLock`; `on_path` reuses
  `resolve_in_path`; `download executable=true` (cross-platform sha256 gate);
  `uninstall_many_with(RunOpts)` (dry-run symmetry); `PluginDecl.path` →
  `recipe_pack` (serde `rename="path"`); `SECURITY.md` updated.
- Verify: **114 tests, 0 failures**; default + `--features cdylib` build with
  **zero warnings**.

## Compatibility audit (the reference installer + sibling projects) — gaps closed

2 parallel Explore audits: the reference installer 12/13 handlers verbatim/equivalent,
`json_catalog` deserializes the real catalog.json losslessly, sentinel kinds
match. The one engine gap (`python:tools`) is **closed**: `python-tools`
recipe (install+uninstall embedded verbatim in `scripts.rs`, `python:` desugar).
"CLI-first ordering" / "supported_clis filter" are caller/wizard concerns (the
strangler adapter keeps the reference installer's wizard+entrypoint) — not engine work.

Sibling projects (mememo/magent/chatgipite/CEAuto/pair-pressure): the dominant
"MCP server → register into ~/.claude.json" pattern works today
(`uv:`/`pip:`/`npm:` + native `merge_json` + `prompt`/`save_input`). Highest-
value gap was Claude skill/agent/command file registration → **closed** with
new **`copy` + `symlink` processors** (cross-platform; Windows symlink→copy
fallback; idempotent). Remaining nice-to-haves (dedicated `build`/docker/
service processors) are expressible via `shell`/`exec` recipes — not blockers.
**116 tests green; default + cdylib build clean.**

## Re-audit #3 (post-generic-refactor) — no regressions, hardening applied

Parallel magent security + debugging review of the current code (marketplace→
recipe, Step::from_json/json_catalog rewrite, EngineCtx, dir/cwd, ensure_line,
copy/symlink). **Debugging: 0 regressions, all 7 changed areas correct, prior
fixes intact.** Security: 0 crit/high, 3 med + 4 low (all new-primitive
robustness). All actioned:
- **json_catalog**: `Vec`→`HashMap` keyed by key + **load-time duplicate-key
  error** across clis/tools/plugins (fixes silent first-match data bug AND the
  O(n) lookup DoS).
- **ensure_line**: rejects `\n`/`\r` in the rendered line (was: re-appended
  every run + rc-line injection).
- **save_input**: `upsert_env_line` rejects `\n`/`\r` in value and `\n`/`\r`/`=`
  in key (was: forged `.env` assignments, e.g. PATH overwrite).
- **symlink**: refuses to `remove_dir_all` a REAL non-symlink directory at
  `dest` (a template/typo `dest` can no longer destroy an unrelated tree).
- **copy_recursive**: skips symlinked entries (symmetric with the archive
  link-refusal; no exfiltration via a planted link in a cloned tree).
- **retries**: strict negative→error + saturate huge (consistent with
  `timeout`; was a silent clamp to 0).
- **`dir`/cwd**: documented in SECURITY.md as a `cd`-equivalent
  trusted-config surface (accepted-by-design; no code).
**127 tests green; default + cdylib build clean.**

## Wizard/pages + progress TUI (W1) — DONE

- Engine: `wizard.rs` — `WizardDef`/`Page`/`Field`, the reference installer-parity
  condition eval, pure `run_wizard` + `Answerer`/`StaticAnswerer` (non-blocking,
  InputResolver-keystone parity), and **`WizardSession`** (pure navigable
  state machine: active-page recompute on every move so back-editing re-gates
  later pages; `progress()`, `submit`/`store`/`back`/`finish`).
- json_catalog: optional `group`/`description`/`default` + `options(kind)`.
- TUI (`insmaller-cli/src/tui.rs`, ratatui 0.29 + crossterm 0.28): progress
  Gauge + breadcrumb header, per-page widgets, **on-screen [◄ Back]/[Next ►]
  buttons AND shortcut keys** (Tab/←→/↑↓/Space/Enter/Esc/q). `BarReporter`
  (indicatif) progress bar for the install phase.
- `setup` subcommand: TTY → TUI; `--answers`/no-TTY → StaticAnswerer
  (unattended, never blocks). `examples/sample.wizard.toml` +
  `sample.answers.toml`; verified end-to-end.
- Fixed: `from_path` bare-relative config path (empty `parent()` canonicalize).
- WizardSession nav fully unit-tested; **136 tests green; default + cdylib
  build clean, no warnings**. TUI rendering is interactive (not unit-tested,
  TTY-gated; the StaticAnswerer path is the tested/CI path).

## Risks (ranked)
1. Processor trait widen — mechanical default, 61-test oracle, isolated first.
2. Windows shell/PATH — unix byte-identical; Windows additive, avoids bash.
3. prompt/Skip strict-undefined — skip-prop designed in P0 before P1 prompt.
4. WASM/cdylib offline deps — feature-gated; core default dep-light; P5 behind flag.
5. Plugin shadowing oracle — core-wins + load-time hard error.
