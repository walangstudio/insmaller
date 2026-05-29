use crate::error::{EngineError, Result};
use crate::step::Step;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// The single engine config (`installer.toml`): processors are built-in code,
/// this declares settings, the default lifecycle, the desugar table, and the
/// recipes. Packages live elsewhere (host `EntrySource`, B5).
#[derive(Debug, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub desugar: Vec<DesugarRule>,
    #[serde(default, rename = "recipe")]
    pub recipes_raw: Vec<RecipeRaw>,
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<PluginDecl>,
    /// Branding/presentation metadata. Never read by install/task logic.
    #[serde(default)]
    pub project: Option<ProjectMeta>,
    /// Named scriptable lifecycle tasks (`[task.run]`, `[task.build]`, …).
    #[serde(default, rename = "task")]
    pub tasks_raw: BTreeMap<String, TaskDef>,
}

/// Branding/presentation strings the CLI/wizard interpolates. The engine MUST
/// NOT read this for install logic; `extra` is opaque pass-through available
/// to task-script templating only.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub about: Option<String>,
    #[serde(default)]
    pub intro_template: Option<String>,
    #[serde(default)]
    pub outro_template: Option<String>,
    #[serde(default)]
    pub default_cli: Option<String>,
    #[serde(default)]
    pub group_order: Vec<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// A named task: an ordered, per-OS, generic [`Step`] pipeline plus simple
/// `needs` composition. The engine knows nothing of what the steps do
/// (Docker/systemd/k8s all live in the script bodies).
#[derive(Debug, Clone, Deserialize)]
pub struct TaskDef {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub steps: Vec<toml::Table>,
    /// Per-OS overrides keyed by `std::env::consts::OS`
    /// ("linux"/"macos"/"windows"); falls back to `steps` when no match.
    #[serde(default)]
    pub os: Option<BTreeMap<String, Vec<toml::Table>>>,
    /// Other tasks to run first (ordered, cycle-guarded).
    #[serde(default)]
    pub needs: Vec<String>,
    /// Opt in to concurrent execution: a `parallel` task may run alongside
    /// other `parallel` tasks whose `needs` are met (throttled by
    /// `settings.max_parallel_tasks`). Default false ⇒ the task runs
    /// exclusively (nothing else runs while it does).
    #[serde(default)]
    pub parallel: bool,
    /// Run only if this predicate holds (same grammar as step `when`). A gated
    /// task is skipped — treated as satisfied so its dependents still run.
    #[serde(default)]
    pub when: Option<String>,
    /// Skip if this predicate holds (inverse of `when`).
    #[serde(default)]
    pub unless: Option<String>,
}

/// Parsed [`TaskDef`]: step tables resolved into `Step`s at load.
#[derive(Debug, Clone)]
pub struct CompiledTask {
    pub description: Option<String>,
    pub steps: Vec<Step>,
    pub os_steps: BTreeMap<String, Vec<Step>>,
    pub needs: Vec<String>,
    pub parallel: bool,
    pub when: Option<String>,
    pub unless: Option<String>,
}

/// A `[[plugin]]` declaration. P2 uses `path` (recipe-pack). `command`/`kinds`
/// (P3 external-process) and `wasm`/`cdylib` (P5) are declared now, wired
/// later. Exactly one transport should be set.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginDecl {
    pub name: String,
    /// Recipe-pack dir (TOML key `path`). Distinct from the transport
    /// fields below — a recipe-pack plugin is data, not a transport.
    #[serde(default, rename = "path")]
    pub recipe_pack: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub wasm: Option<String>,
    #[serde(default)]
    pub cdylib: Option<String>,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub pass_env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default = "default_sentinel_dir")]
    pub sentinel_dir_name: String,
    /// Replaces the hardcoded `enriched_path()` list. Globs allowed; expanded
    /// fresh on each PATH resolve (so nvm-then-npm works without a PATH step).
    #[serde(default)]
    pub path_globs: Vec<String>,
    // ── opt-in hardening (defaults preserve current behavior) ──────────────
    /// false ⇒ disable the `shell_literal` catch-all entirely (a spec with
    /// no matching prefix always errors). Default true (reference parity).
    #[serde(default = "default_true")]
    pub allow_shell_literal: bool,
    /// If non-empty, `download` may only send an `auth_bearer_env` token to a
    /// URL whose `scheme://host[:port]` is in this list (token-exfil guard).
    #[serde(default)]
    pub auth_bearer_allowed_origins: Vec<String>,
    /// true ⇒ a `download` with an executable `mode` MUST set `sha256`.
    #[serde(default)]
    pub require_sha256_for_exec: bool,
    /// TUI palette preset: `default` | `mono` | `high-contrast`. Unknown ⇒
    /// fall back to default. `NO_COLOR`/`INSMALLER_THEME` env override this.
    /// Presentation only; core never interprets it (the CLI maps it).
    #[serde(default)]
    pub theme: Option<String>,
    /// Per-color overrides on top of the chosen preset. Each is `#rrggbb`.
    #[serde(default)]
    pub colors: Option<ThemeColors>,
    /// Sibling catalog path, resolved relative to this config's directory.
    /// Lets one `--config` (or the default `installer.toml`) imply the
    /// catalog so `--catalog` isn't needed. An explicit flag still wins.
    #[serde(default)]
    pub catalog: Option<String>,
    /// Sibling wizard path, same resolution/precedence as `catalog`.
    #[serde(default)]
    pub wizard: Option<String>,
    /// After `setup`, render the resolved vars to a single file a runtime
    /// consumes. Absent ⇒ no-op.
    #[serde(default)]
    pub setup_output: Option<SetupOutput>,
    /// Where install markers live. `global` (default) = today's
    /// `<data_local_dir>/<sentinel_dir_name>`, unchanged. `workspace` =
    /// `<config-dir>/.<sentinel_dir_name>` so a project's installs track with
    /// the project. `sentinel_path` (below) overrides both.
    #[serde(default)]
    pub sentinel_scope: SentinelScope,
    /// Explicit sentinel base (absolute or `~`-expanded). Wins over
    /// `sentinel_scope`. Absent ⇒ scope decides.
    #[serde(default)]
    pub sentinel_path: Option<String>,
    /// true ⇒ `setup` stops after writing `setup_output` + outro and runs no
    /// host install phase. For consumers whose catalog scripts run in a
    /// container/target, not on the machine that ran `setup`.
    #[serde(default)]
    pub setup_writes_config_only: bool,
    /// Windows only: when a `bash` is discoverable on PATH, run shell steps
    /// through it instead of PowerShell. Off by default to preserve the
    /// "Windows recipes are PowerShell" contract; opt in when your catalog's
    /// shell bodies are POSIX (e.g. a Git Bash dependency).
    #[serde(default)]
    pub prefer_bash_on_windows: bool,
    /// TUI: whether catalog group headers start collapsed. Default false
    /// (expanded). `expanded_groups`/`collapsed_groups` override per group.
    #[serde(default)]
    pub start_groups_collapsed: bool,
    /// Group names that always start collapsed (overrides the baseline).
    #[serde(default)]
    pub collapsed_groups: Vec<String>,
    /// Group names that always start expanded (overrides the baseline; wins
    /// over `collapsed_groups`).
    #[serde(default)]
    pub expanded_groups: Vec<String>,
    /// Command run when the binary is invoked with no arguments, e.g.
    /// `"setup"`. Absent ⇒ print usage (the historical behavior). When set,
    /// it also captures the "unknown first token" path so `insmaller
    /// --dry-run` and `insmaller foo` route through this command instead of
    /// the install catch-all.
    #[serde(default)]
    pub default_command: Option<String>,
    /// Args prepended to the user's argv when `default_command` fires. One
    /// element = one argv slot; no shell parsing. Lets a config bake in
    /// baseline flags (`["--answers", "/etc/answers.toml"]`) that the user
    /// can still extend on the command line.
    ///
    /// Precedence note: this config is the one that selects `default_command`
    /// and supplies `default_args`. If `default_args` itself carries a
    /// `--config OTHER`, the dispatched subcommand re-resolves to `OTHER` —
    /// so the routing decision is made from THIS config while the command
    /// then runs against `OTHER`. That two-config "bootstrap → real config"
    /// split is intentional; keep `--config` out of `default_args` unless you
    /// want it.
    #[serde(default)]
    pub default_args: Vec<String>,
    /// Whether task-level `prompt`/`input` steps may read stdin on a TTY.
    /// `None` (default) = auto: on when stdin is a TTY, else env-only. `Some(true)`
    /// = force on (still no-ops without a TTY). `Some(false)` = force off
    /// (env-only everywhere, preserves the pre-0.5 contract).
    #[serde(default)]
    pub interactive_tasks: Option<bool>,
    /// Throttle on how many `parallel = true` tasks run at once. `0` (default)
    /// = unbounded; `n` = at most n concurrently. Concurrency is opt-in per
    /// task (see `[task].parallel`); this only caps it. `needs` ordering is
    /// always honored, and non-`parallel` tasks always run exclusively.
    #[serde(default)]
    pub max_parallel_tasks: usize,
}

/// Sentinel base resolution. `global` keeps the historical per-user location;
/// `workspace` anchors to the discovered config's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SentinelScope {
    #[default]
    Global,
    Workspace,
}

/// Output sink format. Only `env` today; an enum so adding json/toml later is
/// non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Env,
}

/// `[settings.setup_output]` — emit resolved vars to `path`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetupOutput {
    pub path: String,
    #[serde(default)]
    pub format: OutputFormat,
    /// Optional `# ...` header line.
    #[serde(default)]
    pub header: Option<String>,
    /// Allowlist of var names; absent ⇒ all scalar vars.
    #[serde(default)]
    pub include: Option<Vec<String>>,
    /// Unix file mode (e.g. `0o600`).
    #[serde(default)]
    pub mode: Option<u32>,
}

/// Optional hex (`#rrggbb`) overrides for individual palette roles. Core
/// holds them as strings only; the CLI parses them into terminal colors.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeColors {
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub accent_fg: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sentinel_dir_name: default_sentinel_dir(),
            path_globs: vec![],
            allow_shell_literal: true,
            auth_bearer_allowed_origins: vec![],
            require_sha256_for_exec: false,
            theme: None,
            colors: None,
            catalog: None,
            wizard: None,
            setup_output: None,
            sentinel_scope: SentinelScope::default(),
            sentinel_path: None,
            setup_writes_config_only: false,
            prefer_bash_on_windows: false,
            start_groups_collapsed: false,
            collapsed_groups: vec![],
            expanded_groups: vec![],
            default_command: None,
            default_args: vec![],
            interactive_tasks: None,
            max_parallel_tasks: 0,
        }
    }
}

fn default_sentinel_dir() -> String {
    "insmaller".to_string()
}

/// Maps a terse spec prefix to a recipe + how to parse the remainder.
#[derive(Debug, Deserialize)]
pub struct DesugarRule {
    pub prefix: String,
    pub recipe: String,
    pub parse: ParseKind,
}

/// Fixed set of remainder parsers. Each variant RELOCATES the corresponding
/// reference handler's parse logic verbatim (the parity guarantee) — it is
/// not a reimplementation. See `desugar.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseKind {
    /// npm/apt: whole remainder → `packages`.
    RestVerbatim,
    /// uv: `<pkg>[@<git-or-url>]` → `package` (+ optional `from`).
    UvSpec,
    /// git: `<url>:<dest>` rsplit from the right → `url`, `dest`.
    GitUrlDest,
    /// gh: `<owner/repo>:<glob>` → `repo`, `pattern`, `pattern_regex`.
    GhRepoAsset,
    /// nvm: `<version>` → `version`, `install_arg`, `alias_target`.
    NvmVersion,
    /// marketplace: `<repo>:<plugin>[:<mkt>]` → `repo`, `plugin`, `marketplace`.
    MarketplaceSpec,
    /// merge-json: `<path>:<cmd>` → `target`, `command`.
    MergeJsonSpec,
    /// single-token remainder → `arg` (sdkman, corepack, go, …).
    SingleArg,
    /// `name[@version]` → `name`, `version` ("" if absent). Serves pip/cargo/
    /// gem/dotnet/go-install/… whose version syntaxes differ.
    VersionedPkg,
    /// raw `curl … | bash` etc. — whole spec → `script`.
    ShellLiteral,
}

/// Recipe as authored in TOML; step bodies are arrays of tables.
#[derive(Debug, Deserialize)]
pub struct RecipeRaw {
    pub name: String,
    #[serde(default)]
    pub install: Vec<toml::Table>,
    #[serde(default)]
    pub uninstall: Vec<toml::Table>,
    /// Asserted-success steps run after install (the `verify` phase).
    #[serde(default)]
    pub verify: Vec<toml::Table>,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub name: String,
    pub install: Vec<Step>,
    pub uninstall: Vec<Step>,
    pub verify: Vec<Step>,
}

/// Parsed + validated engine config: recipes resolved into `Step`s and
/// indexed by name.
fn build_recipe(r: RecipeRaw) -> Result<Recipe> {
    Ok(Recipe {
        name: r.name.clone(),
        install: r
            .install
            .into_iter()
            .map(Step::from_table)
            .collect::<Result<Vec<_>>>()?,
        uninstall: r
            .uninstall
            .into_iter()
            .map(Step::from_table)
            .collect::<Result<Vec<_>>>()?,
        verify: r
            .verify
            .into_iter()
            .map(Step::from_table)
            .collect::<Result<Vec<_>>>()?,
    })
}

#[derive(Debug)]
pub struct LoadedConfig {
    pub settings: Settings,
    pub desugar: Vec<DesugarRule>,
    /// Raw `[[plugin]]` decls (E1/E4 transports are read here in P3/P5).
    pub plugins: Vec<PluginDecl>,
    /// Presentation metadata (CLI-only; engine never reads this).
    pub project: Option<ProjectMeta>,
    /// Named lifecycle tasks, step pipelines compiled at load.
    pub tasks: BTreeMap<String, CompiledTask>,
    recipes: HashMap<String, Recipe>,
    /// Desugar prefixes claimed by core (immutable; plugins cannot shadow).
    core_prefixes: HashSet<String>,
}

fn compile_tasks(raw: BTreeMap<String, TaskDef>) -> Result<BTreeMap<String, CompiledTask>> {
    let mut out: BTreeMap<String, CompiledTask> = BTreeMap::new();
    for (name, t) in raw {
        let steps = t
            .steps
            .into_iter()
            .map(Step::from_table)
            .collect::<Result<Vec<_>>>()?;
        let mut os_steps = BTreeMap::new();
        if let Some(osm) = t.os {
            for (os, tables) in osm {
                let s = tables
                    .into_iter()
                    .map(Step::from_table)
                    .collect::<Result<Vec<_>>>()?;
                os_steps.insert(os, s);
            }
        }
        out.insert(
            name,
            CompiledTask {
                description: t.description,
                steps,
                os_steps,
                needs: t.needs,
                parallel: t.parallel,
                when: t.when,
                unless: t.unless,
            },
        );
    }
    // `needs` must reference real tasks, with no cycles (DFS, same shape as
    // the orchestrator dep guard).
    for (name, ct) in &out {
        for n in &ct.needs {
            if !out.contains_key(n) {
                return Err(EngineError::Config(format!(
                    "task '{name}' needs unknown task '{n}'"
                )));
            }
        }
    }
    fn visit(
        name: &str,
        tasks: &BTreeMap<String, CompiledTask>,
        stack: &mut Vec<String>,
        done: &mut HashSet<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if stack.iter().any(|s| s == name) {
            return Err(EngineError::Config(format!(
                "task dependency cycle through '{name}'"
            )));
        }
        stack.push(name.to_string());
        for n in &tasks[name].needs {
            visit(n, tasks, stack, done)?;
        }
        stack.pop();
        done.insert(name.to_string());
        Ok(())
    }
    let mut done = HashSet::new();
    for name in out.keys() {
        visit(name, &out, &mut Vec::new(), &mut done)?;
    }
    Ok(out)
}

impl LoadedConfig {
    /// Inline config. Recipe-pack (`path=`) plugins need a base dir, so a
    /// `[[plugin]] path=` is an error here (use `from_path`). Transport
    /// plugins (`command`/`wasm`/`cdylib`) need no base dir and ARE retained,
    /// so `register_external` works with a from_str config.
    #[allow(clippy::should_implement_trait)] // inherent ctor: EngineError, not a FromStr::Err
    pub fn from_str(toml_src: &str) -> Result<Self> {
        let raw: EngineConfig =
            toml::from_str(toml_src).map_err(|e| EngineError::Config(e.to_string()))?;
        let cfg = Self::from_engine_config(raw)?;
        if cfg.plugins.iter().any(|p| p.recipe_pack.is_some()) {
            return Err(EngineError::Config(
                "recipe-pack `[[plugin]] path=` requires LoadedConfig::from_path".into(),
            ));
        }
        Ok(cfg)
    }

    /// Load `installer.toml` and merge every recipe-pack plugin (paths
    /// resolved relative to the toml's directory).
    pub fn from_path(toml_path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(toml_path)
            .map_err(|e| EngineError::Config(format!("{}: {e}", toml_path.display())))?;
        let raw: EngineConfig =
            toml::from_str(&src).map_err(|e| EngineError::Config(e.to_string()))?;
        let plugins = raw.plugins.clone();
        let mut cfg = Self::from_engine_config(raw)?;
        // `parent()` of a bare relative filename is Some("") which can't
        // canonicalize — treat empty/no parent as the current dir.
        let base: &Path = match toml_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let mut plugin_prefix_owner: HashMap<String, String> = HashMap::new();
        let mut seen_plugin: HashSet<String> = HashSet::new();
        for p in &plugins {
            if !seen_plugin.insert(p.name.clone()) {
                return Err(EngineError::Config(format!(
                    "duplicate plugin name '{}'",
                    p.name
                )));
            }
            let Some(rel) = &p.recipe_pack else { continue }; // command/wasm: P3/P5
            // Resolve + bound the plugin dir under the config's directory: a
            // `path = "../../etc"` must not load an arbitrary file.
            let base_canon = base
                .canonicalize()
                .map_err(|e| EngineError::Config(format!("config dir: {e}")))?;
            let pdir = base_canon.join(rel).canonicalize().map_err(|e| {
                EngineError::Config(format!("plugin '{}' path '{rel}': {e}", p.name))
            })?;
            if !pdir.starts_with(&base_canon) {
                return Err(EngineError::Config(format!(
                    "plugin '{}' path '{rel}' escapes the config directory",
                    p.name
                )));
            }
            let pfile = pdir.join("installer.plugin.toml");
            let psrc = std::fs::read_to_string(&pfile).map_err(|e| {
                EngineError::Config(format!("plugin '{}': {}: {e}", p.name, pfile.display()))
            })?;
            let praw: EngineConfig = toml::from_str(&psrc)
                .map_err(|e| EngineError::Config(format!("plugin '{}': {e}", p.name)))?;
            cfg.merge_plugin(&p.name, praw, &mut plugin_prefix_owner)?;
        }
        Self::validate_desugar(&cfg.desugar, &cfg.recipes)?;
        Ok(cfg)
    }

    pub fn from_engine_config(raw: EngineConfig) -> Result<Self> {
        let mut recipes = HashMap::new();
        for r in raw.recipes_raw {
            let name = r.name.clone();
            if recipes.insert(name.clone(), build_recipe(r)?).is_some() {
                return Err(EngineError::Config(format!("duplicate recipe '{name}'")));
            }
        }
        Self::validate_desugar(&raw.desugar, &recipes)?;
        let core_prefixes = raw.desugar.iter().map(|d| d.prefix.clone()).collect();
        let tasks = compile_tasks(raw.tasks_raw)?;
        Ok(Self {
            settings: raw.settings,
            desugar: raw.desugar,
            plugins: raw.plugins,
            project: raw.project,
            tasks,
            recipes,
            core_prefixes,
        })
    }

    /// Every desugar rule must point at a real recipe — caught at load, not
    /// mid-install.
    fn validate_desugar(
        desugar: &[DesugarRule],
        recipes: &HashMap<String, Recipe>,
    ) -> Result<()> {
        for d in desugar {
            if !recipes.contains_key(&d.recipe) {
                return Err(EngineError::Config(format!(
                    "desugar prefix '{}' → unknown recipe '{}'",
                    d.prefix, d.recipe
                )));
            }
        }
        Ok(())
    }

    /// Merge one recipe-pack plugin. Recipes are namespaced `name/<recipe>`
    /// (so they can't collide with core or other plugins); desugar `recipe`
    /// refs are rewritten to the namespaced name, `prefix` stays as authored.
    /// Core always wins a prefix collision (plugin rule dropped); two plugins
    /// claiming the same prefix is a hard error.
    fn merge_plugin(
        &mut self,
        name: &str,
        sub: EngineConfig,
        prefix_owner: &mut HashMap<String, String>,
    ) -> Result<()> {
        for r in sub.recipes_raw {
            let ns = format!("{name}/{}", r.name);
            let mut recipe = build_recipe(r)?;
            recipe.name = ns.clone();
            self.recipes.insert(ns, recipe);
        }
        for mut d in sub.desugar {
            if self.core_prefixes.contains(&d.prefix) {
                continue; // core wins
            }
            if let Some(other) = prefix_owner.get(&d.prefix) {
                return Err(EngineError::Config(format!(
                    "plugins '{other}' and '{name}' both claim spec prefix '{}'",
                    d.prefix
                )));
            }
            prefix_owner.insert(d.prefix.clone(), name.to_string());
            d.recipe = format!("{name}/{}", d.recipe);
            self.desugar.push(d);
        }
        Ok(())
    }

    pub fn recipe(&self, name: &str) -> Option<&Recipe> {
        self.recipes.get(name)
    }

    pub fn recipe_names(&self) -> Vec<&str> {
        self.recipes.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [settings]
        sentinel_dir_name = "demo"
        path_globs = ["~/.local/bin", "~/.nvm/versions/node/*/bin"]

        [[desugar]]
        prefix = "npm:"
        recipe = "npm-global"
        parse = "rest_verbatim"

        [[recipe]]
        name = "npm-global"
        [[recipe.install]]
        type = "exec"
        program = "npm"
        argline = "install -g {{ packages }}"
        [[recipe.uninstall]]
        type = "exec"
        program = "npm"
        argline = "uninstall -g {{ packages }}"
    "#;

    #[test]
    fn loads_and_indexes_recipes() {
        let cfg = LoadedConfig::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.settings.sentinel_dir_name, "demo");
        assert_eq!(cfg.settings.path_globs.len(), 2);
        let r = cfg.recipe("npm-global").unwrap();
        assert_eq!(r.install.len(), 1);
        assert_eq!(r.install[0].kind, "exec");
        assert_eq!(r.uninstall.len(), 1);
        assert_eq!(cfg.desugar[0].parse, ParseKind::RestVerbatim);
    }

    #[test]
    fn defaults_apply_when_sections_absent() {
        let cfg = LoadedConfig::from_str("").unwrap();
        assert_eq!(cfg.settings.sentinel_dir_name, "insmaller");
        assert!(cfg.settings.path_globs.is_empty());
        assert!(cfg.recipe_names().is_empty());
    }

    #[test]
    fn theme_settings_parse_and_default() {
        let cfg = LoadedConfig::from_str("").unwrap();
        assert!(cfg.settings.theme.is_none());
        assert!(cfg.settings.colors.is_none());
        assert!(cfg.settings.catalog.is_none());
        assert!(cfg.settings.wizard.is_none());

        let cfg = LoadedConfig::from_str(
            r##"
            [settings]
            theme = "high-contrast"
            colors = { accent = "#123456", error = "#ff0000" }
            catalog = "demo.catalog.json"
            wizard = "demo.wizard.toml"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.settings.theme.as_deref(), Some("high-contrast"));
        assert_eq!(cfg.settings.catalog.as_deref(), Some("demo.catalog.json"));
        assert_eq!(cfg.settings.wizard.as_deref(), Some("demo.wizard.toml"));
        let c = cfg.settings.colors.unwrap();
        assert_eq!(c.accent.as_deref(), Some("#123456"));
        assert_eq!(c.error.as_deref(), Some("#ff0000"));
        assert!(c.muted.is_none());
    }

    #[test]
    fn desugar_pointing_at_missing_recipe_is_rejected() {
        let bad = r#"
            [[desugar]]
            prefix = "x:"
            recipe = "nope"
            parse = "single_arg"
        "#;
        assert!(LoadedConfig::from_str(bad).is_err());
    }

    #[test]
    fn setup_output_defaults_to_none() {
        let cfg = LoadedConfig::from_str("").unwrap();
        assert!(cfg.settings.setup_output.is_none());
    }

    #[test]
    fn setup_writes_config_only_round_trip() {
        let def = LoadedConfig::from_str("").unwrap();
        assert!(!def.settings.setup_writes_config_only);
        let cfg = LoadedConfig::from_str(
            r#"
            [settings]
            setup_writes_config_only = true
            "#,
        )
        .unwrap();
        assert!(cfg.settings.setup_writes_config_only);
    }

    #[test]
    fn max_parallel_tasks_defaults_to_unbounded() {
        let def = LoadedConfig::from_str("").unwrap();
        assert_eq!(def.settings.max_parallel_tasks, 0);
        let cfg = LoadedConfig::from_str("[settings]\nmax_parallel_tasks = 4\n").unwrap();
        assert_eq!(cfg.settings.max_parallel_tasks, 4);
    }

    #[test]
    fn task_parallel_flag_round_trip() {
        let cfg = LoadedConfig::from_str(
            "[task.a]\nparallel = true\n[[task.a.steps]]\ntype = \"shell\"\nscript = \"x\"\n[task.b]\n[[task.b.steps]]\ntype = \"shell\"\nscript = \"y\"\n",
        )
        .unwrap();
        assert!(cfg.tasks["a"].parallel);
        assert!(!cfg.tasks["b"].parallel, "default is exclusive");
    }

    #[test]
    fn prefer_bash_on_windows_round_trip() {
        let def = LoadedConfig::from_str("").unwrap();
        assert!(!def.settings.prefer_bash_on_windows);
        let cfg =
            LoadedConfig::from_str("[settings]\nprefer_bash_on_windows = true\n").unwrap();
        assert!(cfg.settings.prefer_bash_on_windows);
    }

    #[test]
    fn default_args_round_trip_and_defaults_empty() {
        let def = LoadedConfig::from_str("").unwrap();
        assert!(def.settings.default_args.is_empty());
        let cfg = LoadedConfig::from_str(
            r#"
            [settings]
            default_command = "setup"
            default_args = ["--answers", "/etc/answers.toml"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.settings.default_command.as_deref(), Some("setup"));
        assert_eq!(
            cfg.settings.default_args,
            vec!["--answers".to_string(), "/etc/answers.toml".to_string()]
        );
    }

    #[test]
    fn interactive_tasks_round_trip_tri_state() {
        let def = LoadedConfig::from_str("").unwrap();
        assert!(def.settings.interactive_tasks.is_none());
        let on =
            LoadedConfig::from_str("[settings]\ninteractive_tasks = true\n").unwrap();
        assert_eq!(on.settings.interactive_tasks, Some(true));
        let off =
            LoadedConfig::from_str("[settings]\ninteractive_tasks = false\n").unwrap();
        assert_eq!(off.settings.interactive_tasks, Some(false));
    }

    #[test]
    fn setup_output_all_fields_parse() {
        let cfg = LoadedConfig::from_str(
            r#"
            [settings.setup_output]
            path = "~/.app/out.env"
            format = "env"
            header = "generated"
            include = ["A", "B"]
            mode = 0o600
            "#,
        )
        .unwrap();
        let so = cfg.settings.setup_output.unwrap();
        assert_eq!(so.path, "~/.app/out.env");
        assert_eq!(so.format, OutputFormat::Env);
        assert_eq!(so.header.as_deref(), Some("generated"));
        assert_eq!(so.include.unwrap(), vec!["A", "B"]);
        assert_eq!(so.mode, Some(0o600));
    }

    #[test]
    fn settings_scope_path_parse_and_default_none() {
        let def = LoadedConfig::from_str("").unwrap();
        assert_eq!(def.settings.sentinel_scope, SentinelScope::Global);
        assert!(def.settings.sentinel_path.is_none());
        let cfg = LoadedConfig::from_str(
            r#"
            [settings]
            sentinel_scope = "workspace"
            sentinel_path = "~/.local/share/app"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.settings.sentinel_scope, SentinelScope::Workspace);
        assert_eq!(cfg.settings.sentinel_path.as_deref(), Some("~/.local/share/app"));
    }

    #[test]
    fn project_defaults_to_none() {
        let cfg = LoadedConfig::from_str("").unwrap();
        assert!(cfg.project.is_none());
    }

    #[test]
    fn project_all_fields_parse() {
        let cfg = LoadedConfig::from_str(
            r#"
            [project]
            name = "Demo"
            about = "a demo"
            intro_template = "Hi {{ name }}"
            outro_template = "Bye"
            default_cli = "claude"
            group_order = ["core", "extra"]
            extra = { image_tag = "demo:1", container_name = "demo" }
            "#,
        )
        .unwrap();
        let p = cfg.project.unwrap();
        assert_eq!(p.name.as_deref(), Some("Demo"));
        assert_eq!(p.group_order, vec!["core", "extra"]);
        assert_eq!(p.extra.get("image_tag").unwrap(), "demo:1");
    }

    #[test]
    fn project_extra_is_btreemap() {
        let cfg = LoadedConfig::from_str(
            r#"
            [project.extra]
            z = "1"
            a = "2"
            "#,
        )
        .unwrap();
        let proj = cfg.project.unwrap();
        let keys: Vec<&String> = proj.extra.keys().collect();
        assert_eq!(keys, vec!["a", "z"]); // BTreeMap = sorted
    }

    #[test]
    fn task_parses_basic() {
        let cfg = LoadedConfig::from_str(
            r#"
            [task.build]
            description = "build it"
            [[task.build.steps]]
            type = "shell"
            script = "echo build"
            "#,
        )
        .unwrap();
        let t = cfg.tasks.get("build").unwrap();
        assert_eq!(t.description.as_deref(), Some("build it"));
        assert_eq!(t.steps.len(), 1);
        assert_eq!(t.steps[0].kind, "shell");
    }

    #[test]
    fn task_os_override_parses() {
        let cfg = LoadedConfig::from_str(
            r#"
            [task.run]
            [[task.run.steps]]
            type = "shell"
            script = "echo generic"
            [[task.run.os.linux]]
            type = "shell"
            script = "echo linux"
            [[task.run.os.windows]]
            type = "shell"
            script = "echo win"
            "#,
        )
        .unwrap();
        let t = cfg.tasks.get("run").unwrap();
        assert_eq!(t.os_steps.get("linux").unwrap().len(), 1);
        assert_eq!(t.os_steps.get("windows").unwrap().len(), 1);
    }

    #[test]
    fn task_needs_unknown_is_error() {
        let r = LoadedConfig::from_str(
            r#"
            [task.a]
            needs = ["ghost"]
            [[task.a.steps]]
            type = "shell"
            script = "true"
            "#,
        );
        assert!(format!("{}", r.unwrap_err()).contains("unknown task 'ghost'"));
    }

    #[test]
    fn task_needs_cycle_is_error() {
        let r = LoadedConfig::from_str(
            r#"
            [task.a]
            needs = ["b"]
            [[task.a.steps]]
            type = "shell"
            script = "true"
            [task.b]
            needs = ["a"]
            [[task.b.steps]]
            type = "shell"
            script = "true"
            "#,
        );
        assert!(format!("{}", r.unwrap_err()).contains("cycle"));
    }

    #[test]
    fn task_empty_is_ok() {
        let cfg = LoadedConfig::from_str("").unwrap();
        assert!(cfg.tasks.is_empty());
    }

    #[test]
    fn duplicate_recipe_is_rejected() {
        let dup = r#"
            [[recipe]]
            name = "a"
            [[recipe.install]]
            type = "shell"
            script = "true"

            [[recipe]]
            name = "a"
            [[recipe.install]]
            type = "shell"
            script = "false"
        "#;
        assert!(LoadedConfig::from_str(dup).is_err());
    }
}
