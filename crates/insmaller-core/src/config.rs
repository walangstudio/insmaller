use crate::error::{EngineError, Result};
use crate::step::Step;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
    /// no matching prefix always errors). Default true (codetainyrrr parity).
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
/// codetainyrrr handler's parse logic verbatim (the parity guarantee) — it is
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
    recipes: HashMap<String, Recipe>,
    /// Desugar prefixes claimed by core (immutable; plugins cannot shadow).
    core_prefixes: HashSet<String>,
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
        Ok(Self {
            settings: raw.settings,
            desugar: raw.desugar,
            plugins: raw.plugins,
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
