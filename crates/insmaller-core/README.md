# insmaller-core

The engine behind the [`insmaller`](https://crates.io/crates/insmaller)
installer. It runs declarative, config-driven step pipelines through a registry
of pluggable processors — no terminal, no CLI, no assumptions about where files
live. A host program supplies the packages and drives it.

**Most people want the binary, not this crate** — `cargo install insmaller`.
Reach for `insmaller-core` only when you're embedding the engine in your own
Rust program.

## Add it

```toml
[dependencies]
insmaller-core = "0.6"
```

## What it gives you

- **Step pipelines** — `run_step_pipeline` executes a list of `Step`s (each a
  `type` + params) with `when`/`unless` guards, `requires`, retries, timeouts,
  and a `confirm` gate.
- **Processors** — built-ins (`shell`, `exec`, `download`, `extract`, `copy`,
  `prompt`/`input`, `write_env`, `merge_*`, `backup`, …) registered in a
  `ProcessorRegistry` (`builtins()`); register your own `Processor` impls or
  external plugins.
- **Install / uninstall** — `install_many` / `uninstall_many` drive recipes for
  packages supplied through the `EntrySource` trait, returning an
  `InstallSummary`. Idempotent via a `Sentinel`.
- **Tasks** — `run_tasks` / `run_task` run named `[task.*]` pipelines.
- **Config** — `LoadedConfig::from_path` parses the TOML engine config
  (`[settings]`, `[project]`, recipes, tasks); `Ctx` is the templating/variable
  context steps render against.
- **Inputs** — the `InputResolver` trait abstracts where prompt values come
  from (`EnvResolver` is non-blocking, so unattended runs never hang).

Everything is `async` (Tokio) and dependency-light by design.

## Usage

The [`insmaller` binary crate](https://github.com/walangstudio/insmaller/tree/main/crates/insmaller-cli)
is the canonical reference for wiring these pieces together (config discovery,
an `EntrySource` over a JSON catalog, an interactive resolver, reporters).

Full API docs: <https://docs.rs/insmaller-core>
Project: <https://github.com/walangstudio/insmaller>

License: MIT
