# Wizard fields, validators, and task gating

Reference for everything a consumer can put in a `wizard.toml` field, a catalog
`requires_input` entry, and a `[task.*]`.

## Field types (`type =`)

| type | value | notes |
|------|-------|-------|
| `text` | string | free text |
| `secret` | string | masked in the TUI |
| `path` | string | `Ctrl+B` opens a file/dir browser |
| `single_select` | string | one choice |
| `multiselect` | string[] | many choices; `[x]/[~]/[ ]` group headers |
| `toggle` | bool | on/off |

## Field flags

| flag | type | default | meaning |
|------|------|---------|---------|
| `id` | string | — | variable name (required) |
| `type` | see above | — | required |
| `prompt` | string | id | inline input header / hint shown next to the field |
| `label` | string | prompt→id | concise name for the review page, the post-setup "Answers:" summary, and the question header (precedence `label`→`prompt`→`id`) |
| `default` | string | — | prefill |
| `required` | bool | `true` | must be answered |
| `source` | string | — | `catalog.clis` / `catalog.tools` / `catalog.plugins` (answers are install keys), or `selected.inputs` |
| `options` | string[] | — | static choices (instead of `source`) |
| `condition` | expr | — | show the field only if the predicate holds |

## Validators (text / secret / path fields)

Applied to the scalar value (an empty value is governed by `required`, not these).
Enforced interactively (the TUI re-asks) and on the unattended `--answers` path.

| flag | type | meaning |
|------|------|---------|
| `pattern` | regex | must match in full (auto-anchored `^(?:…)$`) |
| `format` | enum | `integer`, `number`, `alpha` (letters), `alnum`, `email` |
| `min_length` / `max_length` | int | character-count bounds |
| `min` / `max` | number | numeric bounds (value parsed as a number) |
| `error` | string | custom message shown instead of the generated one |

```toml
[[page.field]]
id = "PORT"
type = "text"
format = "integer"
min = 1
max = 65535
required = true

[[page.field]]
id = "NAME"
type = "text"
pattern = "^[a-z][a-z0-9-]+$"
max_length = 32
error = "lowercase letters, digits and dashes"
```

The same validator flags work on a catalog entry's `requires_input` declarations.

## Page flags

`id`, `title`, `description`, `condition`, and `[[page.field]]` entries.

## Condition / `when` grammar

Used by field/page `condition`, step `when`/`unless`, catalog entry `condition`,
and task `when`/`unless`:

```
${VAR} == 'x'      ${VAR} != 'x'      ${VAR} in 'a,b,c'      'item' in ${VAR}
${VAR} >= '20'     <=  >  <            (== / != are semver-aware)
```

## Tasks (`[task.<name>]`)

| flag | type | meaning |
|------|------|---------|
| `steps` | step[] | the pipeline (per-OS overrides via `[[task.x.os.<os>]]`) |
| `needs` | string[] | other tasks that must complete first (cycle-checked at load) |
| `parallel` | bool | may run alongside other `parallel` tasks whose `needs` are met |
| `when` / `unless` | expr | gate the task on a flag; a gated-off task is skipped (treated as satisfied so dependents still run) |

`needs` orders phases; `parallel` opts into concurrency; `when`/`unless` gates on
a value — so `step 1 (sync) → step 2 (parallel) → step 3 (sync)` is just three
tasks wired with `needs`, the middle two marked `parallel`. Concurrency is
throttled by `[settings] max_parallel_tasks` (0 = unbounded); CLI `--jobs N`
overrides it and `--parallel` forces every task parallel.
