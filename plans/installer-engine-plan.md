# insmaller — Config-Driven Installer Engine

Standalone, reusable installer engine extracted in design from codetainyrrr. Replaces
hardcoded `spec→handler` dispatch with declarative **TOML** step pipelines + pluggable
processors. Intended to be consumed by codetainyrrr and other MCP/utils projects via an
`EntrySource`/adapter seam.

Status: **B1–B5 built & green — engine MVP complete, 61 tests pass, CLI smoke OK.**
Standalone engine works end to end. NOT yet integrated into codetainyrrr (that's the
strangler M-series below, deferred). Design below is authoritative.

Built (`cargo test --offline`, 55 lib + 3 config_file + 3 e2e = 61, 0 fail):
- B1 core model: Ctx (minijinja, strict-undefined), Step, Processor, ProcessorRegistry,
  Reporter (+Stdout/Null), InputResolver (+Env/Static), EngineError.
- B2 config: `LoadedConfig` (TOML), recipe/desugar model, 9 `ParseKind` parsers
  RELOCATED verbatim from codetainyrrr handlers (uv/git/gh/marketplace/merge-json/nvm
  parity tests ported and passing); real `installer.toml` validated.
- B3 processors: shell, exec, merge_json (native deep-merge verbatim), check_command,
  claude_plugin (native + guard), sentinel_meta. PATH/expand_home helpers ported
  verbatim. gh-release / go scripts embedded as Rust consts in `scripts.rs` (data-file
  write guard rejects `$(`/`${` inline — so script_file refs resolve to embedded consts).
- B4 orchestrator + `sentinel.rs`: dep resolution, cycle guard, idempotency, `.post`
  gate, `InstallSummary` (collect-not-abort) — all verbatim codetainyrrr semantics.
- B5 `json_catalog.rs` (`EntrySource` over codetainyrrr-shaped catalog.json) +
  `insmaller-cli` (`insmaller install <keys> --config --catalog`, EnvResolver, exit
  code). e2e + CLI smoke (dep order, idempotency, unattended-no-block) green.

Resume point: integrate into codetainyrrr via the M-series (M0 Reporter/InputResolver in
codetainyrrr → … → M4 flip → M5 the crate IS this `insmaller-core`). Differential e2e vs
legacy handlers is the gate.

> Crate naming: the engine crate is **`insmaller-core`**, NOT `installer-core`. Windows'
> Installer Detection heuristic forces a UAC elevation prompt for any executable whose
> name contains "install"/"setup"/"update" (broke `cargo test` with os error 740).
> Wherever this doc says `installer-core`, read `insmaller-core`. Keep "install" out of
> all crate/bin names. Workspace: `crates/insmaller-core` (lib) + `crates/insmaller-cli`
> (bin `insmaller`). Build on this machine needs `cargo … --offline` (network SSL
> revocation check is blocked; codetainyrrr's cargo cache supplies the deps).

---

## 1. Goal

A single TOML engine config defines the installation **flow** (processors, reusable
recipes, lifecycle, global settings — "the basic stuff"). **Packages** live in a separate
extensible config (codetainyrrr's `catalog.json` today). Each package's `install`/
`uninstall` is an ordered list of steps; each step's `type` maps to a generic processor.
No hardcoded per-package rules. Future: extend via custom scripts/plugins.

## 2. Model

- **Step**: `{ type, <params>, when?: predicate, continue_on_error?: bool }` (ordered list
  per package, for `install` and `uninstall`).
- **Processor**: `#[async_trait] trait Processor { async fn run(&self, params:&Value,
  ctx:&Ctx, rep:&dyn Reporter, inp:&dyn InputResolver) -> Result<()> }`. Registry:
  `HashMap<String, Arc<dyn Processor>>`.
- **Recipe**: named, parameterized step sequence in the engine config (reproduces each
  legacy handler **exactly**).
- **Desugar table**: terse spec prefix (`npm:`, `gh:`, `git:`, …) → recipe + a fixed Rust
  parse fn that **relocates the existing handler's parse code verbatim** (this is the
  parity guarantee — parse logic is moved, not reimplemented). So legacy catalog `install`
  strings keep working unchanged; `install` also accepts `{recipe, with}`.
- **Ctx**: read-only `serde_json::Value` var bag (package key, resolved version, os/arch,
  HOME, user inputs). String params rendered via **minijinja** (declared dep, unused
  today) before `run`. Defer `register_as`/`StepOutput` until a real recipe needs it
  (none of the 13 legacy handlers do).
- Dep resolution + sentinel idempotency + `<key>.post` gate stay **engine
  infrastructure**, not steps.

## 3. Built-in processors (migration-minimal set)

Only six ship for the migration. Anything more is feature creep against a green-test goal.

| Processor | Params | Role |
|---|---|---|
| `shell` | `{ script, dir? }` | Workhorse — reproduces nvm, sdkman, github_release, git_clone, go, python, apt, corepack, uv, shell-pipe **verbatim** (run via `run_sh` enriched-PATH semantics). |
| `exec` | `{ program, args[], dir? }` | PATH-resolved spawn (`run_cmd` semantics). For npm. |
| `merge_json` | `{ target, command }` | Native — runs cmd, parses JSON stdout, deep-merges into file. No faithful shell equivalent. Lifts `merge_json.rs` unchanged. |
| `check_command` | `{ program, on_missing }` | PATH probe (`resolve_in_path`); `bail!(on_missing)` on miss. For the marketplace `claude`-present guard. |
| `claude_plugin` | `{ repo, plugin, marketplace? }` | Native — keeps `marketplace add` + `plugin install` + uninstall + guard. |
| `sentinel_meta` | `{}` | Engine-internal, for spec-less meta entries. |

**Explicitly dropped as over-engineering:** `set_path`/`env_append` (PATH is recomputed
fresh each call via `enriched_path()` — keep as infra, parameterized by
`settings.path_globs`); `check_registry`/`check_permission` (speculative — no legacy
handler does pre-flight; `status()` is universally `Missing`); separate
`copy`/`extract`/`symlink` (decomposing `github_release` risks asset-match/chmod parity
drift — ship it as one verbatim shell recipe). Registry enum reserves an `External` arm
(script/binary, step-JSON over stdin) — **designed, not implemented**.

## 4. Keystone — InputResolver (highest risk + its mitigation)

`prompt`/`save_input` must **never block the unattended container** (codetainyrrr's
`entrypoint.rs` has no TTY and `exec`s zsh as PID 1 — a stdin prompt hangs the container
forever, silently, worse than a crash).

```
trait InputResolver: Send + Sync {
    fn resolve(&self, key:&str, spec:&PromptSpec) -> Result<ResolvedInput>;
}
enum ResolvedInput { Value(String), Skip, Fail(String) }
```

- **Container** injects `EnvResolver`: reads env var; missing+required ⇒
  `Fail(...)` → flows into the existing `InstallSummary::failed` banner. **Structurally
  cannot block.** (Mirrors how `CODING_CLI`/`INSTALL_TOOLS` already arrive as env.)
- **Interactive setup/wizard** injects `CliclackResolver`: real prompts.
- `save_input` writes to `.env` (reuse `envfile.rs`) + `Ctx`; in-container is a no-op when
  the value already came from env.

Pair with `trait Reporter` (step start/end, log, progress) replacing the `cliclack::log`
leak at `orchestrator.rs:137` → engine becomes UI-agnostic and library-extractable.

## 5. Engine config — `installer.toml`

TOML (chosen: `toml 0.8` already a dep in codetainyrrr; best for hand-authored recipes +
multi-line shell; zero new deps). Sections:

```toml
[settings]
sentinel_dir_name = "codetainyrrr"
path_globs = [                       # moves enriched_path() hardcoded list out of code
  "~/.local/bin", "~/.cargo/bin", "~/.deno/bin", "~/.bun/bin",
  "~/.dotnet", "~/go/sdk/bin",
  "~/.sdkman/candidates/java/current/bin",
  "~/.nvm/versions/node/*/bin",      # glob expanded each resolve, as today
]

[lifecycle]
phases = ["install", "post_install"]  # dep-resolve + sentinel wrap these, in code

[[desugar]]
prefix = "npm:"
recipe = "npm-global"
parse  = "rest_whitespace"

[[desugar]]
prefix = "gh:"
recipe = "gh-release"
parse  = "split_first_colon"

[[recipe]]
name = "npm-global"
[[recipe.install]]
type = "exec"
program = "npm"
args = ["install", "-g", "{{ packages }}"]
[[recipe.uninstall]]
type = "exec"
program = "npm"
args = ["uninstall", "-g", "{{ packages }}"]

[[recipe]]
name = "gh-release"
[[recipe.install]]
type = "shell"
script = """
set -uo pipefail
REPO="{{ repo }}"; PATTERN_RE="{{ pattern_regex }}"
DEST="$HOME/.local/bin"; mkdir -p "$DEST"
# ... verbatim body of github_release.rs install path ...
"""
[[recipe.uninstall]]
type = "shell"
script = 'rm -f "$HOME/.local/bin/{{ key }}"'
```

`parse` fns are a small fixed Rust enum (`rest_whitespace`, `split_first_colon`,
`rsplit_url_dest`, …), each lifted from the matching handler's parser (incl. transforms
like github_release's `pattern.replace('.', r"\.").replace('*', ".*")`). **Parse logic is
relocated, not rewritten.**

Catalog packages stay JSON; schema gains additive `#[serde(default)]` fields only:
`categories: Vec<Vec<String>>` (multi-path), `version`/`versions`, and
`install: String | {recipe, with}`. Old `catalog.json` parses unchanged.

## 6. Strangler-fig migration (e2e green at every checkpoint)

Invariant: legacy `registry::handler_for` + 13 handlers stay byte-for-byte until the
final flip; the engine runs in parallel, differentially validated, before replacing them.

- **M0** Extract `Reporter` + `InputResolver`; replace the `cliclack::log` leak. No
  behavior change. Checkpoint: full suite green; container banner byte-identical.
- **M1** Engine skeleton + `installer.toml` (all recipes + desugar), **dormant** (no
  callers). Desugar parse fns lift the handler parse unit tests. Checkpoint: green, zero
  callers.
- **M2** `CT_ENGINE=1` env flag routes through engine; differential e2e (engine vs legacy
  ⇒ identical fs + sentinel outcome). Default stays legacy. Checkpoint: both paths green.
- **M3** Per-recipe cutover, simplest→stateful: npm → apt/go/corepack → uv/python →
  git_clone/shell → nvm/sdkman → github_release → merge_json/marketplace. Delete each
  handler as its recipe goes green under the flag.
- **M4** Flip default on; delete `handler_for`/`handlers/`.
- **M5** Extract `crates/installer-core` (the reusable crate); the consuming project keeps
  only a `Catalog→steps` adapter (`EntrySource`).

When integrating back into codetainyrrr, recommended interleave with its Phases:
M0–M2 → (codetainyrrr Phase D) → M3–M4 → (Phase B refine) → (Phase C) → M5.

## 7. Ranked risks

1. **Prompt hanging the unattended container** — mitigated by the `EnvResolver`
   non-blocking contract; add a test: prompt-bearing recipe under `EnvResolver` with var
   unset returns `Err` in ms and the daemon still writes the ready file.
2. **github_release parity drift** — verbatim shell recipe (no decomposition) +
   differential in-container download-diff of `~/.local/bin`.
3. **Sentinel/idempotency regression** — sentinel stays untouched infra wrapping the
   pipeline; `catalog_deps.rs` tests guard after each M-step.
4. **minijinja shell injection** — whitelist interpolated vars in `shell` scripts; raw
   user input only ever via `exec` args (no shell), never `shell`.
5. **catalog schema back-compat** — all new fields `#[serde(default)]`.

## 8. Reusability seam

The engine depends on a `trait EntrySource { fn entry(&self, key) -> Option<EntryRef> }`
(EntryRef = `{ kind, spec|steps, deps, post_install }`). codetainyrrr implements it over
`Catalog` (~20 lines, today's `orchestrator::lookup` body). Other MCP/utils projects
implement their own trivial source. `installer-core` is the publishable artifact (own
repo / registry) once the API stabilizes post-M4.

## 9. Source references (codetainyrrr, for the eventual implementer)

`G:\docker\projs\codetainyrrr\crates\codetainyrrr\src\`:
`installer/registry.rs` (dispatch flip point), `installer/orchestrator.rs`
(Reporter/InputResolver injection; dep+sentinel infra stays),
`installer/handlers/mod.rs` (`enriched_path`/`resolve_in_path`/`run_sh` → settings-driven
helpers), `installer/handlers/*` (parse code to relocate), `cmd/entrypoint.rs`
(non-interactive `EnvResolver` — the hang-risk guard), `config/schema.rs` (additive
fields), `config/loader.rs` (TOML loader to add alongside JSON catalog).

## Open decisions for when this is picked up

- End-state: keep terse `install:` strings forever (desugar always on) vs. eventually
  migrate all catalog entries to explicit `{recipe, with}`. Near-term: keep both.
- Whether the `External` plugin processor ships in the first real build or stays a
  reserved enum arm.
