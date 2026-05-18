# insmaller

![version](https://img.shields.io/badge/version-0.2.1-blue)
![license](https://img.shields.io/badge/license-MIT-green)
![rust](https://img.shields.io/badge/rust-1.78%2B-orange)
![tests](https://img.shields.io/badge/tests-203%20passing-brightgreen)

insmaller installs things by reading a config file instead of running
hand-written install code. You describe each tool as a list of steps in TOML,
point insmaller at the config, and it runs them. It ships as one binary with
nothing else to install.

## Why this exists

This came out of codetainyrrr, which had a Rust function per installable tool.
Every new tool, every package manager, every "also symlink this" meant editing
and recompiling the binary. The install logic and the list of things to install
were the same code.

insmaller separates them. The engine knows how to run a handful of step types
(run a shell snippet, download a file, extract an archive, copy, prompt for
input, and so on). What to install lives in a config file and a catalog. Adding
a tool is editing TOML, not writing Rust, and the same engine can be reused by
other projects through one trait.

One rule shaped a lot of the design: an unattended run, in CI or a container,
must never stop and wait for someone to type an answer. Inputs come from the
environment or a prepared answers file; a missing optional input skips its step
rather than blocking.

## What it does

You give it three things, though usually you only mention one:

- A config (`insmaller.toml`) with the recipes and settings.
- A catalog (`catalog.json`) listing what is installable, either as a short
  spec like `apt:ripgrep` or as inline steps.
- Optionally a wizard (`wizard.toml`) describing selection pages.

The config can name its own catalog and wizard, and the config file itself is
found by walking up from the current directory, the way `.env` or `Cargo.toml`
are. So in a project that has an `insmaller.toml`, you run `insmaller setup`
with no arguments.

It runs the same config across Linux, macOS, and Windows. The OS, architecture,
and package manager are detected at runtime; steps and catalog entries can be
gated on them with a single expression grammar that also does version compares
(`${NODE} >= '20'`); shell snippets run under bash or PowerShell automatically. The bundled recipe packs
cover apt, dnf, pacman, zypper, apk, brew, winget, scoop, choco, and the usual
language installers (pip, pipx, cargo, gem, pnpm, yarn, rustup, asdf, mise,
composer, deno, bun, and more) behind short `name:` prefixes.

Installs are idempotent. A marker is written when a tool installs cleanly, so a
second run skips it. Uninstall behaves the way a real installer should: it only
touches things insmaller installed, it does not remove a tool's dependencies
along with it, and it refuses to remove something another installed tool still
depends on unless you pass `--force`. There is no automatic undo of an install;
a recipe defines its own uninstall steps, and if it does not, uninstall just
clears the marker.

The processors available to steps are shell, exec, download (with sha256 and a
bearer-token guard), extract (tar, zip, gz, bz2, xz, with path-traversal
checks), copy, symlink (a directory junction, then a copy, as fallback on
Windows), merge_json, check_command, prompt, save_input, ensure_line,
write_env, and sentinel_meta. shell/exec/check_command take an optional
`poll = { attempts, delay_ms, until_exit_zero }` for wait-ready loops. Recipes
can also be provided as separate TOML packs, or as external programs that speak
a small JSON protocol.

Beyond installing, the config can declare: per-entry `condition` (offer/skip an
entry on a predicate); `requires_input` on an entry plus a `selected.inputs`
wizard page that collects the union of declared inputs of the selection;
`[settings.setup_output]` to emit the resolved vars to a single env file
atomically; named `[task.*]` lifecycle pipelines (`insmaller task <name>`) with
`needs` ordering and per-OS step overrides; and a `[project]` block of
presentation strings and opaque pass-through `extra` for task templating. All
of it is optional and additive — existing catalogs are unaffected.

## Getting started

```sh
cargo build --release -p insmaller-cli
```

Put the binary on PATH. In your project, write an `insmaller.toml`:

```toml
[settings]
catalog = "catalog.json"
wizard  = "wizard.toml"   # optional
theme   = "default"       # default, mono, or high-contrast
```

Then, from anywhere in that project tree:

```sh
insmaller setup              # interactive wizard, then installs the selection
insmaller ripgrep            # install one thing directly
insmaller uninstall ripgrep  # run its uninstall steps, clear the marker
insmaller task build         # run a [task.build] pipeline (alias: insmaller run)
```

Add `--dry-run` to any of these to see what would happen without doing it.
`--answers FILE`, or simply not having a terminal, makes the run fully
unattended. `--config`, `--catalog`, and `--wizard` override the discovered or
configured paths when you need them. `--force` overrides the uninstall
dependency check.

The [`examples/`](examples/README.md) directory has a self-contained demo that
runs entirely in a temp folder with no network, plus a script that launches the
interactive wizard so you can see it.

## How it is put together

Two crates, one binary, no runtime dependencies and no DLLs:

- `insmaller-core` is the engine. It does not know about a terminal or a
  specific filesystem layout; a host drives it through the `EntrySource` trait.
- `insmaller-cli` is the `insmaller` binary: argument parsing, config
  discovery, the ratatui wizard, and the progress output.

The release binary is a single file of roughly 7 MB. HTTPS uses rustls, the
archive codecs are compiled in, so there is no OpenSSL or zlib to install
alongside it.

## Releases

CI (`.github/workflows/ci.yml`) runs the test suite on Linux, macOS, and
Windows plus clippy on every push and pull request. The Rust toolchain is
pinned in `rust-toolchain.toml` and the workflows use the same version, so
your local build uses the exact compiler and clippy CI does. Run
`scripts/preflight.ps1` (or `scripts/preflight.sh`) before pushing; it runs
CI's test and clippy commands locally so a failure is caught here, not on
GitHub. Failures specific to another OS still cannot be reproduced from one
machine. Bump the version in `rust-toolchain.toml` and the
`dtolnay/rust-toolchain@<ver>` refs in the workflows together.

Releases are cut by `.github/workflows/release.yml`. Push a `vX.Y.Z` tag, or
run the Release workflow from the GitHub Actions tab and give it a tag and a
branch. It creates the tag if you triggered it manually, sets the crate
version from the tag, builds the binary for Linux, macOS (Intel and Apple
silicon), and Windows, and attaches the archives and a SHA256SUMS file to a
GitHub release. `insmaller --version` reports the same version.

## Status

The engine is built and passing: `cargo test --workspace` is 203 tests, no
failures, no ignored, clippy clean. It works on its own through the CLI today.

The optional native plugin transport builds with `--features cdylib`. The WASM
plugin transport is written up but not enabled, because the WASM runtime is not
in this machine's offline build cache; it is one dependency and one transport
away when built online.

What is not done is the migration back into codetainyrrr (the M0 to M5 plan):
insmaller currently stands alone and is not yet wired into it.

- [`SECURITY.md`](SECURITY.md) describes the trust model. The config, catalog,
  and recipes are trusted input, equivalent to running `curl ... | bash`.
- [`plans/`](plans/) has the design and iteration notes, including where each
  piece was lifted from in codetainyrrr.
