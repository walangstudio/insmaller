# insmaller

Config-driven installer. You describe install steps in a TOML file and point
insmaller at it; the engine runs them. Ships as one binary with nothing else to
install.

<sub>*(it's "insmaller" — "inshorter" just didn't sound right.)*</sub>

## Install

```sh
cargo install insmaller
```

Or download a prebuilt binary from the [releases page](https://github.com/walangstudio/insmaller/releases).

## Quick start

Write an `insmaller.toml` (discovered automatically from the current directory
up, or pass `--config`):

```toml
[settings]
# Both optional; only needed for the `setup` wizard / `install`.
catalog = "catalog.json"
wizard  = "wizard.toml"

# A named task: a sequence of steps you run on demand.
[task.hello]
description = "Say hello"

[[task.hello.steps]]
type   = "shell"
script = "echo hello from insmaller"
```

Run it:

```sh
insmaller task hello
```

## Commands

| Command | Does |
|---|---|
| `insmaller setup` | Run the wizard, collect inputs, then install the selection |
| `insmaller install <keys…>` | Install named recipes from the catalog |
| `insmaller uninstall <keys…>` | Reverse an install |
| `insmaller task <name>` (alias `run`) | Run a named `[task.*]` pipeline |
| `insmaller status` (alias `query`) | Show what's installed (`--json` for machine output) |

`[settings] default_command` makes a bare `insmaller` run that command, and
`default_args` prepends baseline arguments.

## How it works

Steps are declarative — `shell`, `exec`, `download`, `extract`, `copy`,
`prompt`/`input`, and more. *What* to install lives in config and a catalog,
not in compiled code; adding a tool is editing TOML.

One rule shaped the design: an unattended run (CI, a container) never stops to
wait for stdin. Inputs come from the environment or a prepared answers file; a
missing optional input skips its step instead of blocking.

The engine is a separate crate, [`insmaller-core`](https://crates.io/crates/insmaller-core),
embeddable in your own Rust program through one trait.

Full documentation: <https://github.com/walangstudio/insmaller>

License: MIT
