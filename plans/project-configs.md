# insmaller — generic-only confirmation, processor inventory, per-project config simulation

## 1. No hardcoded tool logic (confirmed)

`claude_plugin` was the **only** tool-specific native processor — it baked in
`claude plugin marketplace add/install`. It is **removed**. The Claude
marketplace install is now the generic `marketplace` recipe in installer.toml:
`check_command` (guard) + two `exec` steps + an `exec` uninstall. The engine
ships ONLY generic primitives; every tool's behavior is config (recipe/steps).

The only remaining fixed-logic surface is the `ParseKind` desugar enum (9
terse-spec parsers, codetainyrrr-relocated). It is **optional sugar** — a
catalog entry can always use inline `steps` or a `{recipe, with}` ref and never
touch a ParseKind. So the generic path has no fixed logic.

## 2. Processor inventory (11 generic + sentinel_meta infra)

| Processor | Params | Purpose |
|---|---|---|
| `shell` | `script` \| `script_file` | run a script (bash unix / powershell win), enriched PATH |
| `exec` | `program`, `args[]` \| `argline` | run a program (PATH-resolved) |
| `download` | `url`, `dest`, `sha256?`, `auth_bearer_env?`, `mode?`, `executable?`, `timeout?` | fetch URL→file, integrity + token guards |
| `extract` | `archive`, `dest`, `strip_components?` | tar/zip/gz/bz2/xz; symlink & zip-slip refused |
| `copy` | `src`, `dest` | recursive file/dir copy |
| `symlink` | `src`, `dest` | link (Windows → copy fallback); idempotent |
| `merge_json` | `target`, `command` | deep-merge command's JSON stdout into a file |
| `check_command` | `program`, `on_missing?` | assert a binary on PATH, else fail |
| `prompt` | `name`, `env?`, `message?`, `required?`, `secret?` | resolve input via InputResolver (env in unattended; never blocks) |
| `save_input` | `name`, `env?`, `value?`, `file?` | resolve + persist to an env-file + register |
| `sentinel_meta` | — | engine no-op for spec-less meta entries |

Step attributes (all processors): `when`, `unless`, `requires`,
`register_as`, `continue_on_error`, `timeout`, `retries`.
Desugar prefixes (sugar → recipe): `npm: apt: uv: nvm: sdkman: corepack: go:
python: gh: git: merge-json: marketplace:` + sys-pkg pack
(apt/apk/dnf/pacman/zypper/brew/winget/scoop/choco) + lang-pkg pack
(pip/pipx/cargo/gem/dotnet-tool/pnpm/yarn/go-install) + `shell_literal`.
Plugin transports: external-process, cdylib (feature), wasm (deferred).
Host integration: `EntrySource` (`json_catalog` adapter mirrors codetainyrrr).

## 3. Per-project simulated configs (catalog entry + recipe), with gaps

These use the generic `steps` form to prove no tool-specific logic is needed.

### chatgipite / CEAuto (Node MCP server, local repo)
```json
{ "key": "chatgipite", "dependencies": ["node"],
  "steps": [
    {"type":"shell","script":"git clone --depth=1 https://… {{HOME}}/.chatgipite"},
    {"type":"shell","script":"cd {{HOME}}/.chatgipite && npm ci --omit=dev"},
    {"type":"merge_json","target":"{{HOME}}/.claude.json",
     "command":"echo '{\"mcpServers\":{\"chatgipite\":{\"command\":\"node\",\"args\":[\"{{HOME}}/.chatgipite/server.js\"]}}}'"},
    {"type":"symlink","src":"{{HOME}}/.chatgipite/skill","dest":"{{HOME}}/.claude/skills/chatgipite"}
  ]}
```
✅ Fully expressible. **Gap:** the `cd … && npm ci` is a `shell` workaround
because `exec` has no working-directory param (see §4-A).

### magent / mememo (Python MCP server)
```json
{ "key":"mememo", "dependencies":["uv"],
  "steps":[
    {"type":"shell","script":"git clone --depth=1 https://… {{HOME}}/.mememo"},
    {"type":"shell","script":"cd {{HOME}}/.mememo && uv venv && uv pip install -e ."},
    {"type":"exec","program":"claude","argline":"mcp add mememo -- python -m mememo"},
    {"type":"copy","src":"{{HOME}}/.mememo/hooks","dest":"{{HOME}}/.claude/hooks","when":"{{ enable_hooks }}"}
  ]}
```
✅ Fully expressible. Same `cd &&` shell workaround.

### pair-pressure (Python CLI + skill, interactive)
```json
{ "key":"pair-pressure", "dependencies":["uv"],
  "steps":[
    {"type":"prompt","name":"PP_AUTHOR","message":"Your author name:","required":true},
    {"type":"prompt","name":"PP_REPO","message":"Chat repo path:","required":false},
    {"type":"exec","program":"uv","argline":"tool install pair-pressure"},
    {"type":"symlink","src":"…/skill","dest":"{{HOME}}/.claude/skills/pair-pressure"},
    {"type":"save_input","name":"PP_AUTHOR","file":"{{HOME}}/.claude/settings.local.json"}
  ]}
```
✅ prompt/save_input cover the wizard (env-resolved + non-blocking unattended).
**Gap:** writing env exports to `~/.bashrc`/`~/.zshrc` (non-JSON, must be
idempotent) → only `shell` with a grep-guard today (see §4-B).

### gd-skills (Claude skills/agents/commands pack)
```json
{ "key":"gd-skills",
  "steps":[
    {"type":"shell","script":"git clone --depth=1 https://… {{HOME}}/.gd-skills"},
    {"type":"copy","src":"{{HOME}}/.gd-skills/skills","dest":"{{HOME}}/.claude/skills"},
    {"type":"copy","src":"{{HOME}}/.gd-skills/agents","dest":"{{HOME}}/.claude/agents"},
    {"type":"copy","src":"{{HOME}}/.gd-skills/commands","dest":"{{HOME}}/.claude/commands"}
  ]}
```
✅ Fully expressible (copy/symlink added).

### borch / sql-proxy-rs (Rust build-from-source)
```json
{ "key":"borch", "dependencies":["cargo"],
  "steps":[
    {"type":"shell","script":"git clone --depth=1 https://… {{HOME}}/.borch-src"},
    {"type":"shell","script":"cd {{HOME}}/.borch-src && cargo build --release"},
    {"type":"copy","src":"{{HOME}}/.borch-src/target/release/borch","dest":"{{HOME}}/.local/bin/borch"}
  ]}
```
✅ Expressible via `shell`. A dedicated `build` processor would be ergonomics,
not a blocker. sql-proxy-rs adds `exec sqlproxyctl install-service` (its own
CLI generates the systemd/launchd artifact) — expressible.

### sql-proxy (shell + docker, interactive)
```json
{ "key":"sql-proxy",
  "steps":[
    {"type":"shell","script":"git clone --depth=1 https://… {{HOME}}/.sql-proxy"},
    {"type":"prompt","name":"BASTION_HOST","message":"Bastion host:","required":true},
    {"type":"shell","script":"cd {{HOME}}/.sql-proxy && ./sql-proxy config --no-interactive --bastion-host {{ BASTION_HOST }}"},
    {"type":"shell","script":"cd {{HOME}}/.sql-proxy && docker compose up -d"}
  ]}
```
✅ Expressible. Docker/service are `shell`; dedicated processors are
nice-to-have.

## 4. Gaps — CLOSED

- **A. `dir`/`cwd` on `exec`, `shell`, `merge_json` — DONE.** `run_cmd`/
  `run_sh`/`run_capture` take an optional cwd; a step's `dir` (templated +
  home-expanded) sets it. Recipes no longer need `cd … &&` for cloned repos.
- **B. `ensure_line` processor — DONE.** Idempotently ensures `line` in
  `file` (creates parent+file; skip if present). Covers shell rc/profile /
  non-JSON config (pair-pressure env exports).
- **C. (nice-to-have, not gaps)** dedicated `build`/`docker`/`service`
  processors — still expressible via `shell`/`exec` recipes; not needed.

- **D. json_catalog inline `steps` — DONE.** The shipped `EntrySource`
  adapter previously only accepted a terse `install` string (so the generic
  inline-pipeline configs above weren't actually loadable). Added: catalog
  entries take an inline `steps` array (mutually exclusive with `install`),
  pre-parsed at load via the new canonical `Step::from_json` (one parser for
  both TOML recipes and JSON catalog steps). `examples/siblings.catalog.json`
  is a **real, runnable** catalog for mememo/chatgipite/gd-skills/borch/
  pair-pressure using only generic steps; an e2e test loads it against the
  real `installer.toml` and resolves the full dep graph.

**Conclusion:** engine is fully generic (no tool-specific native processor),
the default host adapter supports the fully-generic inline-steps path, and
every sibling installer + codetainyrrr is expressible & verified.
**122 tests green; default + cdylib build clean.**
