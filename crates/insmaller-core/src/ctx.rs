use crate::error::{EngineError, Result};
use minijinja::{Environment, UndefinedBehavior};
use serde_json::{Map, Value};

/// Shared strict-undefined environment (built once; `render_str` is `&self`
/// and the env is stateless across calls). Strict undefined ⇒ a missing var
/// fails loudly instead of rendering an empty shell argument.
fn strict_env() -> &'static Environment<'static> {
    static ENV: std::sync::OnceLock<Environment<'static>> = std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env
    })
}

/// Best-effort probe of the system package manager so recipes can gate with
/// `when = "{{ pkg_manager == 'apk' }}"`. Returns the first found on PATH,
/// OS-biased; "unknown" if none.
fn detect_pkg_manager() -> &'static str {
    let win = &["winget", "scoop", "choco"][..];
    let mac = &["brew"][..];
    let linux = &["apt-get", "dnf", "yum", "zypper", "pacman", "apk"][..];
    let order: Vec<&str> = match std::env::consts::OS {
        "windows" => [win, mac, linux].concat(),
        "macos" => [mac, linux].concat(),
        _ => [linux, mac].concat(),
    };
    for cand in order {
        if on_path(cand) {
            return cand;
        }
    }
    "unknown"
}

fn on_path(program: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    crate::pathenv::resolve_in_path(program, &path).is_some()
}

/// Read-only variable bag carried through a package's step pipeline.
/// Holds the package key, resolved version, os/arch, HOME, and any user
/// inputs. String step params are rendered against this via minijinja
/// before a processor runs.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    vars: Map<String, Value>,
    dry_run: bool,
}

impl Ctx {
    pub fn new() -> Self {
        let mut c = Self::default();
        c.set("os", std::env::consts::OS);
        c.set("arch", std::env::consts::ARCH);
        c.set("os_family", std::env::consts::FAMILY); // "unix" | "windows"
        c.set("exe_ext", std::env::consts::EXE_SUFFIX); // "" | ".exe"
        c.set("pkg_manager", detect_pkg_manager());
        if let Some(home) = dirs::home_dir() {
            c.set("HOME", home.to_string_lossy().as_ref());
        }
        c
    }

    /// True if the var is present (used by `Step::requires` skip logic).
    pub fn has(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }
    pub fn set_dry_run(&mut self, v: bool) -> &mut Self {
        self.dry_run = v;
        self
    }

    /// The full var bag as JSON (sent to external/wasm plugins).
    pub fn vars_json(&self) -> Value {
        Value::Object(self.vars.clone())
    }

    /// A clone with `locals` overlaid — used to layer pipeline-registered
    /// outputs without mutating the shared (read-only) base Ctx.
    pub fn with_locals(&self, locals: &Map<String, Value>) -> Ctx {
        let mut vars = self.vars.clone();
        for (k, v) in locals {
            vars.insert(k.clone(), v.clone());
        }
        Ctx {
            vars,
            dry_run: self.dry_run,
        }
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.vars
            .insert(key.to_string(), Value::String(value.into()));
        self
    }

    pub fn set_value(&mut self, key: &str, value: Value) -> &mut Self {
        self.vars.insert(key.to_string(), value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.vars.get(key)
    }

    /// Render a template string against the context. Non-template strings
    /// pass through unchanged (minijinja leaves literal text as-is).
    pub fn render(&self, template: &str) -> Result<String> {
        let env = strict_env();
        env.render_str(template, &self.vars)
            .map_err(|source| EngineError::Render {
                what: format!("'{template}'"),
                source,
            })
    }

    /// Render with additional locals layered on top of the context
    /// (used to bind recipe params without mutating the shared Ctx).
    pub fn render_with(&self, template: &str, locals: &Map<String, Value>) -> Result<String> {
        let mut merged = self.vars.clone();
        for (k, v) in locals {
            merged.insert(k.clone(), v.clone());
        }
        let env = strict_env();
        env.render_str(template, &merged)
            .map_err(|source| EngineError::Render {
                what: format!("'{template}'"),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_vars() {
        let mut c = Ctx::default();
        c.set("key", "lazygit").set("version", "0.44.1");
        assert_eq!(
            c.render("gh:jesseduffield/{{ key }}@{{ version }}").unwrap(),
            "gh:jesseduffield/lazygit@0.44.1"
        );
    }

    #[test]
    fn literal_passthrough() {
        let c = Ctx::default();
        assert_eq!(c.render("npm install -g foo").unwrap(), "npm install -g foo");
    }

    #[test]
    fn new_populates_platform_probe() {
        let c = Ctx::new();
        let fam = c.get("os_family").unwrap().as_str().unwrap();
        assert!(fam == "unix" || fam == "windows", "got {fam}");
        assert!(c.has("os") && c.has("arch") && c.has("exe_ext"));
        // pkg_manager is always set (possibly "unknown"); render must not error.
        assert!(!c.render("{{ pkg_manager }}").unwrap().is_empty());
    }

    #[test]
    fn has_reflects_presence_and_with_locals_overlay() {
        let mut c = Ctx::default();
        c.set("key", "x");
        assert!(c.has("key") && !c.has("v"));
        let mut locals = Map::new();
        locals.insert("v".into(), Value::String("1".into()));
        let merged = c.with_locals(&locals);
        assert!(merged.has("v") && merged.has("key"));
        assert!(!c.has("v")); // base untouched
    }

    #[test]
    fn render_with_locals_does_not_mutate_ctx() {
        let mut c = Ctx::default();
        c.set("key", "node");
        let mut locals = Map::new();
        locals.insert("packages".into(), Value::String("typescript".into()));
        assert_eq!(
            c.render_with("{{ key }}:{{ packages }}", &locals).unwrap(),
            "node:typescript"
        );
        assert!(c.get("packages").is_none());
    }

    #[test]
    fn missing_var_is_an_error() {
        let c = Ctx::default();
        assert!(c.render("{{ nope }}").is_err());
    }
}
