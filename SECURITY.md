# insmaller — Security Model

## Trust model (read this first)

insmaller executes **declarative recipes that install software**. A recipe can
run shell scripts, spawn processes, download and extract files, and invoke
external/native plugins. **The engine is exactly as trusted as its config,
recipes, catalog, and plugins** — equivalent to running `curl … | bash` from
those sources.

> If you do not trust the `installer.toml`, the recipe packs, the catalog
> (`EntrySource`), or a `[[plugin]]`, do not run them. No engine setting makes
> an untrusted recipe safe.

This matches the origin/reference project where handlers were verbatim
`curl|bash`/`npm`/`git` shells. The engine does not weaken that model; it also
does not pretend to sandbox it.

## Accepted-by-design execution surfaces

These are intentional and equivalent to operator-supplied shell. They are
**not** vulnerabilities under the trust model above:

- **`shell` recipe / `shell_literal`** — runs a script via `bash -c`
  (unix) / `powershell -Command` (windows). Template (`{{ }}`) values are
  interpolated before execution. Catalog values flow into shell here.
- **`exec` `argline`** — whitespace-split into argv (mirrors the reference installer
  `npm` handler). Use the `args` array form when a value may contain spaces
  or when the program is itself an interpreter.
- **`post_install`** — raw shell commands, run once via the platform shell.
- **step `dir`** — a templated, home-expanded working directory for
  `exec`/`shell`/`merge_json` (equivalent to `cd` in a shell recipe). It is an
  execution-context surface: a `dir` assembled from a `register_as`/`prompt`
  value runs the step in that directory and resolves relative `dest` paths
  against it. Trusted-config only; for less-trusted catalogs, treat inline
  `steps` with `dir` as you would a `shell` step.
- **`merge_json` `command`** — runs a command to produce JSON; it is a shell
  surface (now platform-aware, no hardcoded `bash`).
- **External / cdylib plugins** — arbitrary code with the engine's
  privileges. `sandbox = true` only trims the inherited environment; it is a
  hygiene control, **not** a security boundary. A cdylib plugin that violates
  the FFI/allocator contract can corrupt the engine process.

## Hardening that IS enforced (always on)

- **Archive extraction (`extract`)** rejects path traversal: `..`, absolute
  paths, and Windows prefixes are refused (`safe_join`); tar **symlink and
  hardlink entries are refused** (zip-slip / link-traversal closed).
- **`copy` / `symlink`** write to a recipe-controlled `dest` (home-expanded);
  this is intended (e.g. registering skills into `~/.claude/skills/`) and is
  inside the trusted-config boundary, same as a `shell` recipe. `symlink` on
  Windows falls back to a copy when symlink privilege is absent.
- **Recipe-pack plugin paths** (`[[plugin]] path=`) are canonicalized and must
  resolve **inside the config directory**; `../` escapes are rejected at load.
- **Core-wins**: a recipe-pack plugin can never shadow a core recipe or claim
  a core spec prefix; plugin↔plugin prefix collisions are a hard load error.
- **Strict templating**: a missing template variable is a hard error, never a
  silently empty shell argument.
- **`shell_literal` guard**: a spec with no matching prefix is only treated as
  a shell pipeline when it actually looks like one (`curl`/`wget`/`sh`/`bash`/
  `| sh`/`| bash`); a typo like `cargo:ripgrep` errors instead of executing.
- **External plugins**: subprocess child is `kill_on_drop` (engine timeout
  actually kills it); the JSON protocol version is checked and refused loudly
  on mismatch.

## Opt-in hardening switches (`[settings]`)

Defaults preserve current behavior; tighten per deployment:

```toml
[settings]
# false ⇒ disable the shell_literal catch-all entirely. Any spec with no
# matching desugar prefix becomes a hard error (no implicit shell exec).
allow_shell_literal = false

# If non-empty, `download` may only send an `auth_bearer_env` token to a URL
# whose scheme://host[:port] is listed here (token-exfiltration guard). A
# bearer token additionally always requires https://.
auth_bearer_allowed_origins = ["https://api.github.com", "https://github.com"]

# true ⇒ any `download` that is executable MUST also set `sha256`
# (supply-chain integrity). "Executable" = a unix exec `mode` (e.g. 0o755)
# OR `executable = true` on the step (cross-platform; Windows ignores mode).
require_sha256_for_exec = true
```

Recommended for any deployment consuming catalogs/recipes from a source less
trusted than the operator: set all three.

## Reporting

This is an internal tool. File issues in the project tracker; there is no
external disclosure process.
