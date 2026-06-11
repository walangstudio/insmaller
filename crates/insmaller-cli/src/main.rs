//! Standalone harness for the engine.
//!
//!   insmaller <key…>            [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller install   <key…> [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller uninstall <key…> [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller setup [--wizard F] [--catalog F] [--config F] [--answers F] [--dry-run] [--run|--no-run]
//!   insmaller status [<key>] [--config F] [--json]
//!
//! insmaller is an installer: a bare `insmaller <key…>` (no recognized
//! subcommand) defaults to `install`. `uninstall` runs each recipe's
//! `uninstall` phase and clears its sentinels. `setup` runs the pages/wizard,
//! then installs the selected keys (wizard string answers are exported to the
//! env so prompt/save_input/EnvResolver pick them up). `--answers` makes
//! `setup` fully unattended (non-blocking).

mod interactive;
mod theme;
mod tui;

use insmaller_core::{
    builtins, parse_env_file_to_map, run_wizard, Catalog, Ctx, EnvResolver, FieldType,
    InputResolver, InstallSummary, LoadedConfig, Reporter, Sentinel, SentinelData, Settings,
    StaticAnswerer, StdoutReporter, WizardDef, WizardOutcome, WizardSession,
};
use serde_json::{Map, Value};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn opt(args: &[String], flag: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}
/// `Some(value)` only if the flag was actually passed (lets a config-supplied
/// path fill in when it wasn't).
fn opt_opt(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Config filenames auto-discovered when `--config` is omitted, in priority
/// order. `insmaller.toml` is the recommended name; `installer.toml` is the
/// legacy default kept for back-compat.
const CONFIG_NAMES: &[&str] = &["insmaller.toml", ".insmaller.toml", "installer.toml"];

/// First `dir/name` (closest dir wins, then `names` order) for which `exists`
/// is true, walking `start` and its ancestors — `.env`/`Cargo.toml`-style.
/// `exists` is injected so the walk is unit-testable without touching disk.
fn find_config(start: &Path, names: &[&str], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        names
            .iter()
            .map(|n| dir.join(n))
            .find(|cand| exists(cand))
    })
}

/// Program name derived from argv0 — `Path::file_stem` strips `.exe`; falls
/// back to `"insmaller"` when argv0 is missing/unreadable. Lets a rebranded
/// copy (binary renamed to `mytool`) report and discover under its own
/// name without recompilation.
fn program_name_from(argv0: Option<&str>) -> String {
    argv0
        .map(std::path::Path::new)
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "insmaller".into())
}

fn program_name() -> String {
    program_name_from(std::env::args().next().as_deref())
}

/// POSIX app-home candidates for `<name>/installer.toml` (XDG → ~/.<name> →
/// /etc/<name>). Pure: env reads + `dirs::*` happen in the caller and inject
/// the resolved bases, so tests don't touch global state. Compiled on every
/// platform so cross-platform tests can call it; production wiring is
/// `cfg(unix)`-gated.
#[cfg_attr(not(unix), allow(dead_code))]
fn app_home_candidates_posix(
    name: &str,
    xdg_config: Option<&str>,
    config_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // An empty-but-set env var (e.g. `XDG_CONFIG_HOME=`) is "unset" per the XDG
    // spec; without this it would yield a bogus *relative* candidate and shadow
    // the `dirs::config_dir()` fallback.
    let base = xdg_config
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| config_dir.map(PathBuf::from));
    if let Some(b) = base {
        out.push(b.join(name).join("installer.toml"));
    }
    if let Some(h) = home_dir {
        out.push(h.join(format!(".{name}")).join("installer.toml"));
    }
    out.push(PathBuf::from("/etc").join(name).join("installer.toml"));
    out
}

/// Windows app-home candidates for `<name>\installer.toml` (`%APPDATA%` →
/// `%USERPROFILE%\.<name>` → `%PROGRAMDATA%`). Same purity contract as the
/// POSIX variant; cross-compiled for tests.
#[cfg_attr(not(windows), allow(dead_code))]
fn app_home_candidates_windows(
    name: &str,
    appdata: Option<&str>,
    config_dir: Option<&Path>,
    home_dir: Option<&Path>,
    program_data: Option<&str>,
    data_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    // Empty-but-set `%APPDATA%`/`%PROGRAMDATA%` is treated as unset (see POSIX
    // note) so it doesn't shadow the `dirs::*` fallback with a relative path.
    let base = appdata
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| config_dir.map(PathBuf::from));
    if let Some(b) = base {
        out.push(b.join(name).join("installer.toml"));
    }
    if let Some(h) = home_dir {
        out.push(h.join(format!(".{name}")).join("installer.toml"));
    }
    let sysbase = program_data
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| data_dir.map(PathBuf::from));
    if let Some(b) = sysbase {
        out.push(b.join(name).join("installer.toml"));
    }
    out
}

/// Production wiring for `app_home_candidates_*` — reads live env/`dirs::*`.
fn app_home_candidates(name: &str) -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        let xdg = std::env::var("XDG_CONFIG_HOME").ok();
        app_home_candidates_posix(
            name,
            xdg.as_deref(),
            dirs::config_dir().as_deref(),
            dirs::home_dir().as_deref(),
        )
    }
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").ok();
        let progdata = std::env::var("PROGRAMDATA").ok();
        app_home_candidates_windows(
            name,
            appdata.as_deref(),
            dirs::config_dir().as_deref(),
            dirs::home_dir().as_deref(),
            progdata.as_deref(),
            dirs::data_dir().as_deref(),
        )
    }
}

/// The config path sitting next to the running binary
/// (`dir(current_exe())/installer.toml`). Lets a freshly-extracted bundle run
/// in place find its own sibling recipe from any cwd. Only the legacy
/// `installer.toml` name is probed (a bundle ships exactly that, and matching
/// only it keeps a stray `insmaller.toml` in a shared bin dir from hijacking
/// discovery). `current_exe()` failure → empty (the tier is silently skipped).
fn exe_sibling_candidates() -> Vec<PathBuf> {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .map(|dir| vec![dir.join("installer.toml")])
        .unwrap_or_default()
}

/// `--config` if given (wins over everything), else cwd+ancestors discovery,
/// else the exe-sibling candidates (a bundle run in place), else app-home
/// candidates derived from `<name>`, else the legacy `installer.toml` literal
/// (so a missing-file error names something sensible). cwd-ancestors stays
/// above exe-sibling so a project-local config still wins.
fn discover_config_in(
    explicit: Option<String>,
    cwd: &Path,
    exe_sibling: &[PathBuf],
    app_home: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> String {
    if let Some(e) = explicit {
        return e;
    }
    if let Some(p) = find_config(cwd, CONFIG_NAMES, &exists) {
        return p.to_string_lossy().into_owned();
    }
    for cand in exe_sibling.iter().chain(app_home) {
        if exists(cand) {
            return cand.to_string_lossy().into_owned();
        }
    }
    "installer.toml".to_string()
}

fn discover_config(explicit: Option<String>, name: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_sibling = exe_sibling_candidates();
    let app_home = app_home_candidates(name);
    discover_config_in(explicit, &cwd, &exe_sibling, &app_home, |p| p.is_file())
}

/// Resolve a sibling file (catalog/wizard). Precedence: explicit `flag` →
/// `[settings]` value (relative to the config's own directory) → cwd default.
fn resolve_sibling(
    flag: Option<String>,
    from_cfg: Option<&str>,
    cfg_p: &str,
    default: &str,
) -> String {
    if let Some(f) = flag {
        return f;
    }
    if let Some(rel) = from_cfg {
        return std::path::Path::new(cfg_p)
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""))
            .join(rel)
            .to_string_lossy()
            .into_owned();
    }
    default.to_string()
}

// Interactive answering is the ratatui TUI (see `tui.rs`); the unattended
// path uses the engine's non-blocking StaticAnswerer.

/// Usage text rendered for `<name>`. Kept side-effect-free so tests can
/// assert the basename leaks through to every line.
fn usage_text(name: &str) -> String {
    format!(
        "usage:\n  {name} <key…>            [--config F] [--catalog F] [--dry-run] [--json]   (defaults to install)\n  {name} install   <key…> [--config F] [--catalog F] [--dry-run] [--json]\n  {name} uninstall <key…> [--config F] [--catalog F] [--dry-run] [--json] [--force]\n  {name} setup [--wizard F] [--catalog F] [--config F] [--answers F] [--dry-run] [--run|--no-run]\n  {name} task <name…>     [--config F]   (alias: {name} run <name…>)\n  {name} status [<key>]   [--config F] [--json]   (alias: {name} query)\n\n--config: if omitted, the first of insmaller.toml/.insmaller.toml/\ninstaller.toml found in the cwd or any parent dir; failing that, an\ninstaller.toml sitting next to the binary (so an extracted bundle finds its\nown recipe from any cwd); failing that, an app-home location derived from the\nprogram name (e.g. ~/.{name}/installer.toml on POSIX,\n%APPDATA%\\{name}\\installer.toml on Windows).\n--catalog/--wizard default to the config's `[settings] catalog`/`wizard`\n(relative to the config file) if set, else catalog.json/wizard.toml in cwd.\n--force: uninstall even if another installed key still depends on it.\ntask: runs a `[task.<name>]` pipeline (needs first, per-OS, fail-fast)."
    )
}

#[tokio::main]
async fn main() -> ExitCode {
    let name = program_name();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = args.first().map(String::as_str) {
        match first {
            "install" => return cmd_op(&args[1..], Op::Install, &name).await,
            "uninstall" | "remove" => return cmd_op(&args[1..], Op::Uninstall, &name).await,
            "setup" => return cmd_setup(&args[1..], &name).await,
            "task" | "run" => return cmd_task(&args[1..], &name).await,
            "status" | "query" => return cmd_status(&args[1..], &name).await,
            "-V" | "--version" | "version" => {
                let engine_version = env!("CARGO_PKG_VERSION");
                let color =
                    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                let path = discover_config(None, &name);
                let project = insmaller_core::probe_project_meta(Path::new(&path));
                println!(
                    "{}",
                    insmaller_core::render_about(project.as_ref(), &name, engine_version, color)
                );
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" | "help" => {
                println!("{}", usage_text(&name));
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    // No recognized subcommand. If `[settings] default_command` is set,
    // route through it (with `default_args` prepended) so a configured
    // default absorbs both the bare-invocation and the unknown-arg cases —
    // `insmaller`, `insmaller --dry-run`, and `insmaller foo` all reach the
    // same place. Falls back to the historical behavior (bare = usage+fail,
    // unknown = install catch-all) when no default is configured.
    //
    // `--config` must be honored at THIS layer too — otherwise a user
    // pointing the binary at a custom config gets dispatch driven by an
    // unrelated sibling/parent `installer.toml`.
    let cfg_p = discover_config(opt_opt(&args, "--config"), &name);
    let (default_cmd, default_args) = peek_default_dispatch(&cfg_p);
    if let Some(cmd) = default_cmd {
        // Skip the chain+collect allocation when there's nothing to prepend.
        let effective: Vec<String> = if default_args.is_empty() {
            args
        } else {
            default_args.into_iter().chain(args).collect()
        };
        return dispatch_named(&cmd, &effective, &name).await;
    }
    if args.is_empty() {
        eprintln!("{}", usage_text(&name));
        return ExitCode::FAILURE;
    }
    cmd_op(&args, Op::Install, &name).await
}

/// Read ONLY the dispatch-relevant settings (`default_command`/`default_args`)
/// from the discovered config, avoiding a full `LoadedConfig` build (recipe
/// indexing, plugin merge, sibling-path resolution, cross-ref) at the dispatch
/// layer — the chosen `cmd_*` does the one authoritative load. A malformed or
/// unreadable existing config warns (so a syntax error isn't invisible here),
/// then falls back to "no default". An absent config is silent.
fn peek_default_dispatch(cfg_p: &str) -> (Option<String>, Vec<String>) {
    if !Path::new(cfg_p).exists() {
        return (None, vec![]);
    }
    let parsed = std::fs::read_to_string(cfg_p)
        .map_err(|e| e.to_string())
        .and_then(|s| insmaller_core::peek_dispatch_settings(&s).map_err(|e| e.to_string()));
    match parsed {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("config load warning ({cfg_p}): {e}");
            (None, vec![])
        }
    }
}

/// Shared dispatch from a command name string to the matching cmd_* function.
/// Used by both the default-command path and any future caller (e.g. recipes
/// that want to invoke a sibling command). Unknown names are a config error
/// (the validation point is here, not at parse time, so a missing config
/// still gets a sensible error).
async fn dispatch_named(cmd: &str, args: &[String], name: &str) -> ExitCode {
    match cmd {
        "setup" => cmd_setup(args, name).await,
        "install" => cmd_op(args, Op::Install, name).await,
        "uninstall" | "remove" => cmd_op(args, Op::Uninstall, name).await,
        "task" | "run" => cmd_task(args, name).await,
        "status" | "query" => cmd_status(args, name).await,
        other => {
            eprintln!("config error: unknown default_command '{other}'");
            eprintln!("{}", usage_text(name));
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy)]
enum Op {
    Install,
    Uninstall,
}

async fn load(
    cfg_p: &str,
    cat_flag: Option<String>,
) -> insmaller_core::Result<(LoadedConfig, Catalog)> {
    let cfg = LoadedConfig::from_path(std::path::Path::new(cfg_p))?;
    let cat_p = resolve_sibling(cat_flag, cfg.settings.catalog.as_deref(), cfg_p, "catalog.json");
    let cat = Catalog::from_json_str(&std::fs::read_to_string(&cat_p)?)?;
    Ok((cfg, cat))
}

// Runs install/uninstall and returns the summary plus its result noun. The
// caller owns the Reporter (Stdout/Json/Bar) and prints the summary after,
// so the indicatif bar can be cleared before the summary lines.
/// Scope-aware sentinel: `[settings] sentinel_path`/`sentinel_scope` decide
/// the base; default is the historical per-user location, unchanged. The
/// config's own directory anchors `workspace` scope.
fn sentinel_for(cfg: &LoadedConfig, cfg_p: &str) -> Sentinel {
    Sentinel::resolve(&cfg.settings, Path::new(cfg_p).parent())
}

/// Whether this resolver belongs to an interactive task pipeline (where
/// auto-on prompting is the documented default) or an install/uninstall
/// operation (where the pre-0.5 contract was env-only and any TTY prompting
/// has to be opt-in to avoid surprising existing CI scripts that ran
/// `insmaller install` with stdin attached).
#[derive(Clone, Copy)]
enum ResolverPurpose {
    Task,
    Operation,
}

/// `prompt`/`input` resolver chosen from `[settings] interactive_tasks` and
/// the resolver's purpose. `Some(false)` ⇒ env-only everywhere (the
/// opt-out). `Some(true)` ⇒ interactive everywhere (force-on). `None`
/// (default) ⇒ interactive only for tasks; install/uninstall stay env-only
/// so a `prompt` step in an install recipe still fails fast on missing env,
/// preserving the historical fail-fast contract. The wrapped fallback is
/// always `EnvResolver`, so the structurally-non-blocking guarantee holds
/// whichever branch is taken.
fn make_resolver(settings: &Settings, purpose: ResolverPurpose) -> Box<dyn InputResolver> {
    let want_interactive = match settings.interactive_tasks {
        Some(true) => true,
        Some(false) => false,
        None => matches!(purpose, ResolverPurpose::Task),
    };
    if want_interactive {
        Box::new(interactive::InteractiveResolver::new(
            interactive::RealIo,
            EnvResolver,
        ))
    } else {
        Box::new(EnvResolver)
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_op(
    cfg: &LoadedConfig,
    cat: &Catalog,
    keys: &[String],
    op: Op,
    dry_run: bool,
    force: bool,
    rep: &dyn Reporter,
    run_vars: Option<&Map<String, Value>>,
    cfg_p: &str,
    purpose: ResolverPurpose,
) -> (InstallSummary, &'static str) {
    let mut reg = builtins(&cfg.settings);
    insmaller_core::register_external(&mut reg, &cfg.plugins);
    let sent = sentinel_for(cfg, cfg_p);
    let opts = insmaller_core::RunOpts { dry_run, force };
    let resolver = make_resolver(&cfg.settings, purpose);
    match op {
        Op::Install => (
            insmaller_core::install_many_with(
                cat, cfg, &reg, rep, resolver.as_ref(), &sent, keys, opts, run_vars,
            )
            .await,
            "ok",
        ),
        Op::Uninstall => (
            insmaller_core::uninstall_many_with(
                cat, cfg, &reg, rep, resolver.as_ref(), &sent, keys, opts,
            )
            .await,
            "uninstalled",
        ),
    }
}

fn summarize(s: &InstallSummary, verb: &str) -> ExitCode {
    println!(
        "\n── summary: {} {}, {} failed ──",
        s.completed.len(),
        verb,
        s.failed.len()
    );
    for (k, e) in &s.failed {
        println!("  FAILED {k}: {e}");
    }
    if s.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// Non-flag tokens are keys; flag values are skipped.
fn collect_keys(a: &[String]) -> Vec<String> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--config" | "--catalog" | "--jobs" | "-j" => i += 2,
            "--dry-run" | "--json" | "--force" | "--parallel" | "-p" | "--no-api-validate" => i += 1,
            k => {
                keys.push(k.to_string());
                i += 1;
            }
        }
    }
    keys
}

async fn cmd_op(a: &[String], op: Op, name: &str) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"), name);
    let keys = collect_keys(a);
    match load(&cfg_p, opt_opt(a, "--catalog")).await {
        Ok((cfg, cat)) => {
            let rep: Box<dyn Reporter> = if has(a, "--json") {
                Box::new(insmaller_core::JsonReporter)
            } else {
                Box::new(StdoutReporter)
            };
            let (s, verb) = run_op(
                &cfg,
                &cat,
                &keys,
                op,
                has(a, "--dry-run"),
                has(a, "--force"),
                rep.as_ref(),
                None,
                &cfg_p,
                ResolverPurpose::Operation,
            )
            .await;
            summarize(&s, verb)
        }
        Err(e) => {
            eprintln!("config error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `setup_then_task`: after `setup` finishes, optionally prompt (default-yes)
/// and run a lifecycle task in-process with the wizard's collected answers.
/// `interactive` is the TTY context (the wizard TUI ran); on a non-TTY run we
/// only proceed if `--run` was passed. `--no-run` always skips. Returns the
/// task's exit code (so a failed run surfaces), or SUCCESS when nothing ran.
async fn maybe_run_then_task(
    a: &[String],
    cfg: &LoadedConfig,
    vars: &Map<String, Value>,
    interactive: bool,
) -> ExitCode {
    let Some(task) = cfg.settings.setup_then_task.clone() else {
        return ExitCode::SUCCESS;
    };
    if has(a, "--no-run") {
        return ExitCode::SUCCESS;
    }
    let forced = has(a, "--run");
    // Non-interactive (e.g. --answers / piped) only auto-runs when forced.
    if !interactive && !forced {
        return ExitCode::SUCCESS;
    }
    if !cfg.tasks.contains_key(&task) {
        // setup itself succeeded; a misconfigured hook shouldn't fail it.
        eprintln!("setup_then_task: no such task '{task}' — skipping");
        return ExitCode::SUCCESS;
    }
    if !forced {
        let product = cfg
            .project
            .as_ref()
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| task.clone());
        let prompt = cfg
            .settings
            .setup_then_task_prompt
            .clone()
            .unwrap_or_else(|| "Run {product} now?".to_string())
            .replace("{product}", &product);
        print!("{prompt} [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return ExitCode::SUCCESS;
        }
        match line.trim().to_lowercase().as_str() {
            "n" | "no" => return ExitCode::SUCCESS,
            _ => {}
        }
    }
    // run_vars: wizard answers win, then project.extra, then env, then exe vars
    // (mirrors cmd_task so the task templates resolve identically).
    let mut run_vars: Map<String, Value> = Map::new();
    for (k, v) in vars {
        run_vars.insert(k.clone(), v.clone());
    }
    if let Some(proj) = cfg.project.as_ref() {
        for (k, v) in &proj.extra {
            run_vars
                .entry(k.clone())
                .or_insert_with(|| Value::String(v.clone()));
        }
    }
    for (k, v) in std::env::vars() {
        run_vars.entry(k).or_insert(Value::String(v));
    }
    inject_exe_vars(&mut run_vars, std::env::current_exe().ok());
    let mut reg = builtins(&cfg.settings);
    insmaller_core::register_external(&mut reg, &cfg.plugins);
    let resolver = make_resolver(&cfg.settings, ResolverPurpose::Task);
    match insmaller_core::run_tasks(
        &[task],
        cfg,
        &reg,
        &StdoutReporter,
        resolver.as_ref(),
        &run_vars,
        cfg.settings.max_parallel_tasks,
        false,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("task failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_setup(a: &[String], name: &str) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"), name);
    let wiz_flag = opt_opt(a, "--wizard");
    let (cfg, cat) = match load(&cfg_p, opt_opt(a, "--catalog")).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wiz_p = resolve_sibling(wiz_flag, cfg.settings.wizard.as_deref(), &cfg_p, "wizard.toml");
    let wiz = match std::fs::read_to_string(&wiz_p)
        .map_err(|e| e.to_string())
        .and_then(|s| WizardDef::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("wizard error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = insmaller_core::validate_wizard_schema(&wiz) {
        eprintln!("wizard error: {e}");
        return ExitCode::FAILURE;
    }

    // --answers F → non-blocking StaticAnswerer; else interactive stdin.
    // Unattended (--answers or no TTY) → non-blocking StaticAnswerer.
    // Interactive TTY → the ratatui progress TUI (back/forward + buttons).
    // P2-A: render the intro template (project.extra as vars) at setup start.
    let group_order: Vec<String> = cfg
        .project
        .as_ref()
        .map(|p| p.group_order.clone())
        .unwrap_or_default();
    if let Some(proj) = cfg.project.as_ref() {
        if let Some(tmpl) = &proj.intro_template {
            let mut c = Ctx::new();
            for (k, v) in &proj.extra {
                c.set(k, v.as_str());
            }
            if let Ok(s) = c.render(tmpl) {
                println!("{s}");
            }
        }
    }

    let palette = theme::Palette::resolve(&cfg.settings);
    let no_api_validate = has(a, "--no-api-validate");
    let unattended = has(a, "--answers") || !std::io::stdin().is_terminal();
    let (mut outcome, tui_used, final_defaults_map): (
        WizardOutcome,
        bool,
        std::collections::HashMap<String, String>,
    ) = if unattended {
        let f = opt(a, "--answers", "answers.toml");
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        let mut m: Map<String, Value> = toml::from_str::<toml::Table>(&raw)
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_object().cloned())
            .or_else(|| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        // For the unattended path, pre-apply defaults_from_file if the path
        // resolves against the answers map (answers supply the template vars).
        let dff_defaults = cfg.settings.defaults_from_file.as_deref().map(|tmpl| {
            load_defaults_from_file_with_vars(tmpl, &m)
        }).unwrap_or_default();
        // Merge: answers file wins; defaults fill in gaps.
        for (k, v) in &dff_defaults {
            m.entry(k.clone()).or_insert_with(|| Value::String(v.clone()));
        }
        match run_wizard(&wiz, &cat, &StaticAnswerer(m), &group_order) {
            Ok(o) => (o, false, dff_defaults),
            Err(e) => {
                eprintln!("wizard error: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let mut session = WizardSession::new(&wiz, &cat, group_order.clone());
        // Wire up defaults_from_file lazy loading.
        if let Some(tmpl) = cfg.settings.defaults_from_file.clone() {
            session.set_defaults_from_file(tmpl);
        }
        let gd = tui::GroupDefaults {
            collapsed_default: cfg.settings.start_groups_collapsed,
            collapsed: cfg.settings.collapsed_groups.clone(),
            expanded: cfg.settings.expanded_groups.clone(),
        };
        match tui::run_wizard_tui(&mut session, palette, &gd, no_api_validate) {
            Ok(true) => {
                let dm = session.defaults_map().clone();
                (session.finish(), true, dm)
            }
            Ok(false) => {
                println!("Setup cancelled.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("tui error: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    // Drop transient fields (wizard navigation/control flags like a new/edit
    // mode selector). They drove conditions during the wizard but must not be
    // seeded into the env, written to setup_output, shown in the summary, or
    // handed to a follow-up setup_then_task.
    {
        let transient_ids: std::collections::HashSet<&str> = wiz
            .pages
            .iter()
            .flat_map(|p| p.fields.iter())
            .filter(|f| f.transient)
            .map(|f| f.id.as_str())
            .collect();
        if !transient_ids.is_empty() {
            outcome.vars.retain(|k, _| !transient_ids.contains(k.as_str()));
        }
    }

    // Seed scalar answers into the env so prompt/save_input/EnvResolver use
    // them, then install the selected keys.
    for (k, v) in &outcome.vars {
        match v {
            Value::String(s) => std::env::set_var(k, s),
            Value::Bool(b) => std::env::set_var(k, b.to_string()),
            _ => {}
        }
    }

    // P1-C: emit the resolved vars to the configured sink (runs before the
    // install phase so it's present even on --dry-run).
    if let Some(so) = cfg.settings.setup_output.as_ref() {
        // Before writing, substitute __KEEP__ back to the original secret values
        // from the defaults map so we never write the sentinel to disk.
        let mut write_vars = outcome.vars.clone();
        for (k, v) in write_vars.iter_mut() {
            if v.as_str() == Some("__KEEP__") {
                if let Some(orig) = final_defaults_map.get(k) {
                    *v = Value::String(orig.clone());
                }
            }
        }
        // Never persist a leftover sentinel: a kept secret is substituted above
        // (its original is in the defaults map); any remaining __KEEP__ is a
        // stray (e.g. a non-secret field literally set to it) — drop the key
        // rather than write the sentinel string to disk.
        write_vars.retain(|_, v| v.as_str() != Some("__KEEP__"));
        if let Err(e) = insmaller_core::write_setup_output(so, &write_vars) {
            eprintln!("setup_output error: {e:#}");
            return ExitCode::FAILURE;
        }
    }

    // P2-A: outro rendered at the end (project.extra + scalar wizard vars).
    let render_outro = || {
        if let Some(proj) = cfg.project.as_ref() {
            if let Some(tmpl) = &proj.outro_template {
                let mut c = Ctx::new();
                for (k, v) in &proj.extra {
                    c.set(k, v.as_str());
                }
                for (k, v) in &outcome.vars {
                    if let Value::String(s) = v {
                        c.set(k, s.as_str());
                    }
                }
                if let Ok(s) = c.render(tmpl) {
                    println!("{s}");
                }
            }
        }
    };

    // Collect secret field ids so we can mask their values in the summary.
    // Cover both static wizard fields AND catalog requires_input declarations
    // (selected.inputs expansion path), which are not in wiz.pages.
    let mut secret_ids: std::collections::HashSet<String> = wiz
        .pages
        .iter()
        .flat_map(|p| p.fields.iter())
        .filter(|f| f.field_type == FieldType::Secret)
        .map(|f| f.id.clone())
        .collect();
    for decl in cat.required_inputs(&outcome.selected_keys) {
        if decl.r#type == FieldType::Secret {
            secret_ids.insert(decl.id.clone());
        }
    }

    if !outcome.vars.is_empty() {
        // Map each var id to its display label (label → prompt → id) so the
        // summary reads "Container runtime = podman", not the raw env-var key.
        let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for f in wiz.pages.iter().flat_map(|p| p.fields.iter()) {
            labels.insert(f.id.clone(), f.display_label().to_string());
        }
        for decl in cat.required_inputs(&outcome.selected_keys) {
            let label = decl
                .label
                .as_deref()
                .or(decl.prompt.as_deref())
                .unwrap_or(&decl.id)
                .to_string();
            labels.entry(decl.id.clone()).or_insert(label);
        }
        println!("Answers:");
        let mut keys: Vec<&String> = outcome.vars.keys().collect();
        keys.sort();
        for k in keys {
            if let Some(v) = outcome.vars.get(k) {
                let display = format_answer_value(v, secret_ids.contains(k));
                let label = labels.get(k).map(|s| s.as_str()).unwrap_or(k.as_str());
                println!("  {label} = {display}");
            }
        }
    }

    println!("Selected: {:?}", outcome.selected_keys);
    if outcome.selected_keys.is_empty() {
        if outcome.vars.is_empty() {
            println!("Nothing selected.");
        } else {
            println!("No packages to install (answers recorded above).");
        }
        render_outro();
        return maybe_run_then_task(a, &cfg, &outcome.vars, tui_used).await;
    }
    // config-only consumers (install runs in their container/target): stop after
    // setup_output + outro, run zero host install scripts.
    if cfg.settings.setup_writes_config_only {
        render_outro();
        return maybe_run_then_task(a, &cfg, &outcome.vars, tui_used).await;
    }
    let dry_run = has(a, "--dry-run");
    // An interactively-run setup (the wizard TUI was used) is a TTY context
    // where a `prompt` step in an install recipe should actually prompt —
    // unless the user opted out with `interactive_tasks = false`. So the
    // install phase gets ResolverPurpose::Task there, not the env-only
    // Operation default that bare `insmaller install` uses.
    let interactive_setup = tui_used && cfg.settings.interactive_tasks != Some(false);
    let purpose = if interactive_setup {
        ResolverPurpose::Task
    } else {
        ResolverPurpose::Operation
    };
    // The indicatif spinner (120 ms repaint) and a masked prompt fight over
    // the same stdout, so the spinner is used ONLY when we won't prompt:
    // the opted-out interactive case. The unattended path and any
    // prompt-capable interactive path use the plain reporter.
    if !tui_used || interactive_setup {
        let (s, verb) = run_op(
            &cfg,
            &cat,
            &outcome.selected_keys,
            Op::Install,
            dry_run,
            false,
            &StdoutReporter,
            Some(&outcome.vars),
            &cfg_p,
            purpose,
        )
        .await;
        let code = summarize(&s, verb);
        render_outro();
        if s.failed.is_empty() {
            return maybe_run_then_task(a, &cfg, &outcome.vars, tui_used).await;
        }
        return code;
    }
    // tui_used + interactive_tasks == Some(false): env-only, so no prompt can
    // fire — the spinner is safe. The bar must be cleared before the summary
    // prints, hence run_op → finish → summarize.
    let bar = tui::BarReporter::new(palette);
    let (s, verb) = run_op(
        &cfg,
        &cat,
        &outcome.selected_keys,
        Op::Install,
        dry_run,
        false,
        &bar,
        Some(&outcome.vars),
        &cfg_p,
        ResolverPurpose::Operation,
    )
    .await;
    bar.finish();
    let code = summarize(&s, verb);
    render_outro();
    if s.failed.is_empty() {
        return maybe_run_then_task(a, &cfg, &outcome.vars, tui_used).await;
    }
    code
}

/// Resolve `defaults_from_file` path template against the answers map (for the
/// unattended path where the wizard hasn't run yet).  Substitutes `${VAR}` from
/// `answers`, home-expands the result, reads + parses the file.  Returns an
/// empty map on any error (missing file, unresolved vars, etc.).
fn load_defaults_from_file_with_vars(
    template: &str,
    answers: &Map<String, Value>,
) -> std::collections::HashMap<String, String> {
    // Shared `${VAR}` resolver (same logic the interactive session uses):
    // None = a placeholder is unresolved/empty → no defaults.
    let path = match insmaller_core::resolve_dollar_template(template, answers) {
        Some(p) => p,
        None => return std::collections::HashMap::new(),
    };
    let expanded = match insmaller_core::pathenv::expand_home(&path) {
        Ok(p) => p,
        Err(_) => return std::collections::HashMap::new(),
    };
    let content = std::fs::read_to_string(&expanded).unwrap_or_default();
    if content.is_empty() {
        return std::collections::HashMap::new();
    }
    parse_env_file_to_map(&content)
}

/// Inject `self_exe`/`exe_dir` task vars from the running binary's path so a
/// recipe can `copy {{ self_exe }}` and `{{ exe_dir }}/payload/*` from any cwd.
/// `or_insert` so an existing project.extra/env value of the same name wins.
/// `exe = None` (`current_exe()` failed) → injects nothing, no panic.
fn inject_exe_vars(run_vars: &mut Map<String, Value>, exe: Option<PathBuf>) {
    let Some(exe) = exe else { return };
    // `parent()` is `Some("")` for a bare filename — skip that degenerate dir
    // so `{{ exe_dir }}/x` never renders to a bogus `/x`.
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        run_vars
            .entry("exe_dir".to_string())
            .or_insert_with(|| Value::String(dir.to_string_lossy().into_owned()));
    }
    run_vars
        .entry("self_exe".to_string())
        .or_insert_with(|| Value::String(exe.to_string_lossy().into_owned()));
}

/// `insmaller task <name…>` / `insmaller run <name…>` — run named lifecycle
/// task pipelines. `run_vars` = project.extra + process env + self_exe/exe_dir
/// (so task scripts template the consumer's image_tag/container_name/etc and
/// the running binary's own location).
///
/// `CT_ARG` positional argument: a trailing token that is NOT a known
/// `[task.*]` name is treated as a positional argument and exposed to all task
/// shell steps as the env var `CT_ARG`.  Disambiguation rule:
///
/// - tokens matching a known task name → task list (run in sequence)
/// - a single non-task token in the LAST position (every earlier token is a
///   task) → CT_ARG
/// - an unknown token anywhere else (e.g. a leading typo) stays in the task
///   list so the run path errors naming it — never silently promoted to CT_ARG
///
/// Backward compatible: no trailing non-task token → CT_ARG not set (scripts
/// read `${CT_ARG:-}` and fall back to their own defaults).
fn partition_task_tokens(
    tokens: &[String],
    is_task: impl Fn(&str) -> bool,
) -> (Vec<String>, Option<String>) {
    if tokens.is_empty() {
        return (vec![], None);
    }
    if tokens.iter().all(|t| is_task(t)) {
        return (tokens.to_vec(), None);
    }
    // Accept the last token as the positional arg only when every preceding
    // token is a known task (so `run work-claude` works, `work-claude run` errors).
    let (head, last) = tokens.split_at(tokens.len() - 1);
    if !is_task(&last[0]) && head.iter().all(|t| is_task(t)) {
        return (head.to_vec(), Some(last[0].clone()));
    }
    (tokens.to_vec(), None)
}

async fn cmd_task(a: &[String], name: &str) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"), name);
    let raw_tokens = collect_keys(a);
    let cfg = match LoadedConfig::from_path(std::path::Path::new(&cfg_p)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Partition tokens into the task-run list + an optional trailing CT_ARG.
    let (task_names, ct_arg) = partition_task_tokens(&raw_tokens, |t| cfg.tasks.contains_key(t));

    if task_names.is_empty() {
        eprintln!("usage: {name} task <name…>  (available: {:?})", cfg.tasks.keys().collect::<Vec<_>>());
        return ExitCode::FAILURE;
    }
    let mut run_vars: Map<String, Value> = Map::new();
    if let Some(proj) = cfg.project.as_ref() {
        for (k, v) in &proj.extra {
            run_vars.insert(k.clone(), Value::String(v.clone()));
        }
    }
    for (k, v) in std::env::vars() {
        run_vars.entry(k).or_insert(Value::String(v));
    }
    inject_exe_vars(&mut run_vars, std::env::current_exe().ok());

    // Inject CT_ARG so shell steps can read the positional argument as
    // `${CT_ARG:-}` (falls back to empty when not set).
    // Two channels:
    //   1. run_vars → Ctx → available as {{ CT_ARG }} in templated step params.
    //   2. std::env::set_var → inherited by subprocesses spawned by run_sh/run_cmd,
    //      so plain `$CT_ARG` in shell script bodies works too.
    if let Some(arg) = ct_arg {
        run_vars.insert("CT_ARG".to_string(), Value::String(arg.clone()));
        // SAFETY: set before run_tasks; no concurrent threads mutate the env here.
        #[allow(deprecated)]
        std::env::set_var("CT_ARG", &arg);
    }

    let mut reg = builtins(&cfg.settings);
    insmaller_core::register_external(&mut reg, &cfg.plugins);

    // Concurrency is opt-in per task (`[task].parallel`). `--parallel`/`-p`
    // forces every task to behave as parallel for this run; `--jobs N`/`-j`
    // throttles concurrent parallel tasks (overriding max_parallel_tasks).
    let force_parallel = has(a, "--parallel") || has(a, "-p");
    let max_parallel = match opt_opt(a, "--jobs").or_else(|| opt_opt(a, "-j")) {
        Some(j) => match j.parse::<usize>() {
            Ok(0) => {
                eprintln!("--jobs must be >= 1 (use 1 for sequential; omit it for the configured default)");
                return ExitCode::FAILURE;
            }
            Ok(n) => n,
            Err(_) => {
                eprintln!("--jobs expects a number, got '{j}'");
                return ExitCode::FAILURE;
            }
        },
        None => cfg.settings.max_parallel_tasks,
    };

    let resolver = make_resolver(&cfg.settings, ResolverPurpose::Task);
    match insmaller_core::run_tasks(
        &task_names,
        &cfg,
        &reg,
        &StdoutReporter,
        resolver.as_ref(),
        &run_vars,
        max_parallel,
        force_parallel,
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("task failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `insmaller status [<key>]` — read-only listing of what the scope-aware
/// sentinel records as installed. `--json` emits an array; otherwise an
/// aligned table. Optional positional filters to one key. Always SUCCESS
/// (empty is not an error); only a config load failure is FAILURE.
async fn cmd_status(a: &[String], name: &str) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"), name);
    let cfg = match LoadedConfig::from_path(std::path::Path::new(&cfg_p)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let filter = collect_keys(a).into_iter().next();
    let sent = sentinel_for(&cfg, &cfg_p);
    let mut rows: Vec<(String, String, SentinelData, bool)> = sent
        .installed()
        .into_iter()
        .filter(|(_, k)| filter.as_ref().is_none_or(|f| f == k))
        .filter_map(|(kind, key)| {
            sent.read(&kind, &key).map(|d| {
                let post = sent.post_install_done(&kind, &key);
                (kind, key, d, post)
            })
        })
        .collect();
    rows.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));

    if has(a, "--json") {
        let arr: Vec<Value> = rows
            .iter()
            .map(|(kind, key, d, post)| {
                serde_json::json!({
                    "kind": kind, "key": key,
                    "version": d.version, "spec": d.spec,
                    "installed_at": d.installed_at, "post_done": post,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr)).unwrap());
        return ExitCode::SUCCESS;
    }
    if rows.is_empty() {
        println!("nothing installed");
        return ExitCode::SUCCESS;
    }
    let (kw, yw) = rows
        .iter()
        .fold((4, 3), |(k, y), r| (k.max(r.0.len()), y.max(r.1.len())));
    println!("{:<kw$}  {:<yw$}  {:<10}  spec", "kind", "key", "version");
    for (kind, key, d, post) in &rows {
        println!(
            "{:<kw$}  {:<yw$}  {:<10}  {}{}",
            kind,
            key,
            d.version.as_deref().unwrap_or("-"),
            d.spec,
            if *post { "  (post-install done)" } else { "" }
        );
    }
    ExitCode::SUCCESS
}

/// Format a single answer value for display. Secrets are replaced with `***`;
/// arrays join their string elements with `", "`.
fn format_answer_value(v: &Value, is_secret: bool) -> String {
    if is_secret {
        return "***".to_string();
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        app_home_candidates_posix, app_home_candidates_windows, discover_config_in,
        find_config, format_answer_value, inject_exe_vars, partition_task_tokens,
        program_name_from, resolve_sibling, usage_text, CONFIG_NAMES,
    };
    use serde_json::{Map, Value};
    use std::path::{Path, PathBuf};

    fn toks(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    // Known tasks for the partition tests.
    fn is_known(t: &str) -> bool {
        ["run", "build", "stop"].contains(&t)
    }

    #[test]
    fn ct_arg_trailing_non_task_becomes_arg() {
        let (tasks, arg) = partition_task_tokens(&toks(&["run", "work-claude"]), is_known);
        assert_eq!(tasks, toks(&["run"]));
        assert_eq!(arg.as_deref(), Some("work-claude"));
    }

    #[test]
    fn ct_arg_leading_unknown_stays_in_task_list() {
        // A non-task token that is NOT trailing must NOT become CT_ARG; it stays
        // so the run path errors naming the unknown task.
        let (tasks, arg) = partition_task_tokens(&toks(&["work-claude", "run"]), is_known);
        assert_eq!(tasks, toks(&["work-claude", "run"]));
        assert_eq!(arg, None);
    }

    #[test]
    fn ct_arg_multiple_known_tasks_no_arg() {
        let (tasks, arg) = partition_task_tokens(&toks(&["build", "run"]), is_known);
        assert_eq!(tasks, toks(&["build", "run"]));
        assert_eq!(arg, None);
    }

    #[test]
    fn ct_arg_single_task_no_arg() {
        let (tasks, arg) = partition_task_tokens(&toks(&["run"]), is_known);
        assert_eq!(tasks, toks(&["run"]));
        assert_eq!(arg, None);
    }

    #[test]
    fn ct_arg_multiple_unknown_no_arg() {
        // Two unknowns → not promoted; kept so the run path errors.
        let (tasks, arg) = partition_task_tokens(&toks(&["run", "foo", "bar"]), is_known);
        assert_eq!(tasks, toks(&["run", "foo", "bar"]));
        assert_eq!(arg, None);
    }

    #[test]
    fn find_config_closest_dir_wins_then_name_priority() {
        let present: Vec<PathBuf> = vec![
            "/a/b/installer.toml".into(),
            "/a/b/insmaller.toml".into(),
            "/a/.insmaller.toml".into(),
        ];
        let got = find_config(Path::new("/a/b/c"), CONFIG_NAMES, |p| {
            present.iter().any(|q| q == p)
        });
        // /a/b is closer than /a; within it insmaller.toml outranks installer.toml
        assert_eq!(got, Some(PathBuf::from("/a/b/insmaller.toml")));
    }

    #[test]
    fn find_config_walks_up_to_ancestor() {
        let got = find_config(Path::new("/x/y/z"), CONFIG_NAMES, |p| {
            p == Path::new("/x/.insmaller.toml")
        });
        assert_eq!(got, Some(PathBuf::from("/x/.insmaller.toml")));
    }

    #[test]
    fn find_config_none_when_absent_everywhere() {
        assert_eq!(
            find_config(Path::new("/p/q"), CONFIG_NAMES, |_| false),
            None
        );
    }

    #[test]
    fn flag_wins_over_config_and_default() {
        assert_eq!(
            resolve_sibling(Some("x.json".into()), Some("c.json"), "examples/i.toml", "catalog.json"),
            "x.json"
        );
    }

    #[test]
    fn config_value_resolves_relative_to_config_dir() {
        let got = resolve_sibling(None, Some("demo.catalog.json"), "examples/demo.installer.toml", "catalog.json");
        assert_eq!(
            got.replace('\\', "/"),
            "examples/demo.catalog.json"
        );
    }

    #[test]
    fn bare_config_name_has_no_dir_prefix() {
        let got = resolve_sibling(None, Some("demo.catalog.json"), "installer.toml", "catalog.json");
        assert_eq!(got, "demo.catalog.json");
    }

    #[test]
    fn falls_back_to_cwd_default_when_unset() {
        assert_eq!(
            resolve_sibling(None, None, "installer.toml", "catalog.json"),
            "catalog.json"
        );
    }

    // ── P4: program name + app-home discovery ─────────────────────────────

    #[test]
    fn program_name_strips_exe_and_dir() {
        assert_eq!(program_name_from(Some("/usr/local/bin/mytool")), "mytool");
        // `\` is only a path separator on Windows; on Unix the whole string is
        // one component, so file_stem can't strip the dir/`.exe` here.
        #[cfg(windows)]
        assert_eq!(program_name_from(Some(r"C:\bin\mytool.exe")), "mytool");
        assert_eq!(program_name_from(Some("insmaller")), "insmaller");
    }

    #[test]
    fn program_name_falls_back_to_insmaller() {
        assert_eq!(program_name_from(None), "insmaller");
        assert_eq!(program_name_from(Some("")), "insmaller");
    }

    #[test]
    fn explicit_config_flag_wins_over_everything() {
        let app_home = vec![PathBuf::from("/home/u/.mytool/installer.toml")];
        let got = discover_config_in(
            Some("/tmp/explicit.toml".into()),
            Path::new("/cwd"),
            &[PathBuf::from("/b/installer.toml")],
            &app_home,
            |_| true, // every path exists
        );
        assert_eq!(got, "/tmp/explicit.toml");
    }

    #[test]
    fn app_home_discovered_when_only_app_home_present() {
        // argv0=mytool, only ~/.mytool/installer.toml on disk → discovered.
        let app_home = vec![PathBuf::from("/home/u/.mytool/installer.toml")];
        let got = discover_config_in(
            None,
            Path::new("/some/cwd"),
            &[],
            &app_home,
            |p| p == Path::new("/home/u/.mytool/installer.toml"),
        );
        assert_eq!(got, "/home/u/.mytool/installer.toml");
    }

    #[test]
    fn cwd_wins_over_app_home_when_both_present() {
        let app_home = vec![PathBuf::from("/home/u/.mytool/installer.toml")];
        let cwd_cfg = PathBuf::from("/proj/installer.toml");
        let present = vec![cwd_cfg.clone(), app_home[0].clone()];
        let got = discover_config_in(
            None,
            Path::new("/proj"),
            &[],
            &app_home,
            |p| present.iter().any(|q| q == p),
        );
        // Path::join uses platform separator; normalize for portable assert.
        assert_eq!(got.replace('\\', "/"), "/proj/installer.toml");
    }

    #[test]
    fn falls_back_to_legacy_default_when_nothing_found() {
        // argv0=insmaller (default) and no app-home dir → existing behavior unchanged.
        let got = discover_config_in(None, Path::new("/p/q"), &[], &[], |_| false);
        assert_eq!(got, "installer.toml");
    }

    // ── S1: exe-sibling config discovery ──────────────────────────────────

    #[test]
    fn exe_sibling_discovered_from_unrelated_cwd() {
        // bin at /b/mytool + /b/installer.toml, cwd=/elsewhere, no --config.
        let exe_sibling = vec![PathBuf::from("/b/installer.toml")];
        let got = discover_config_in(
            None,
            Path::new("/elsewhere"),
            &exe_sibling,
            &[PathBuf::from("/home/u/.mytool/installer.toml")],
            |p| p == Path::new("/b/installer.toml"),
        );
        assert_eq!(got, "/b/installer.toml");
    }

    #[test]
    fn cwd_wins_over_exe_sibling_when_both_present() {
        let exe_sibling = vec![PathBuf::from("/b/installer.toml")];
        let present = [
            PathBuf::from("/proj/installer.toml"),
            PathBuf::from("/b/installer.toml"),
        ];
        let got = discover_config_in(
            None,
            Path::new("/proj"),
            &exe_sibling,
            &[],
            |p| present.iter().any(|q| q == p),
        );
        assert_eq!(got.replace('\\', "/"), "/proj/installer.toml");
    }

    #[test]
    fn exe_sibling_wins_over_app_home_when_both_present() {
        let exe_sibling = vec![PathBuf::from("/b/installer.toml")];
        let app_home = vec![PathBuf::from("/home/u/.mytool/installer.toml")];
        let present = [exe_sibling[0].clone(), app_home[0].clone()];
        let got = discover_config_in(
            None,
            Path::new("/elsewhere"),
            &exe_sibling,
            &app_home,
            |p| present.iter().any(|q| q == p),
        );
        assert_eq!(got, "/b/installer.toml");
    }

    #[test]
    fn explicit_config_wins_over_exe_sibling() {
        let exe_sibling = vec![PathBuf::from("/b/installer.toml")];
        let got = discover_config_in(
            Some("/tmp/x.toml".into()),
            Path::new("/elsewhere"),
            &exe_sibling,
            &[],
            |_| true,
        );
        assert_eq!(got, "/tmp/x.toml");
    }

    // ── S2: self_exe / exe_dir task vars ──────────────────────────────────

    #[test]
    fn inject_exe_vars_sets_self_exe_and_exe_dir() {
        let mut rv: Map<String, Value> = Map::new();
        inject_exe_vars(&mut rv, Some(PathBuf::from("/b/mytool")));
        assert_eq!(rv.get("self_exe").and_then(Value::as_str), Some("/b/mytool"));
        assert_eq!(rv.get("exe_dir").and_then(Value::as_str), Some("/b"));
    }

    #[test]
    fn inject_exe_vars_preserves_existing_override() {
        // a project.extra/env value of the same name must win.
        let mut rv: Map<String, Value> = Map::new();
        rv.insert("self_exe".into(), Value::String("/override/bin".into()));
        rv.insert("exe_dir".into(), Value::String("/override".into()));
        inject_exe_vars(&mut rv, Some(PathBuf::from("/b/mytool")));
        assert_eq!(rv.get("self_exe").and_then(Value::as_str), Some("/override/bin"));
        assert_eq!(rv.get("exe_dir").and_then(Value::as_str), Some("/override"));
    }

    #[test]
    fn inject_exe_vars_noop_when_exe_unknown() {
        let mut rv: Map<String, Value> = Map::new();
        inject_exe_vars(&mut rv, None);
        assert!(rv.is_empty());
    }

    #[test]
    fn inject_exe_vars_skips_empty_exe_dir_for_bare_name() {
        // current_exe() returning a bare filename → parent() is Some("");
        // exe_dir must be omitted, not injected as "".
        let mut rv: Map<String, Value> = Map::new();
        inject_exe_vars(&mut rv, Some(PathBuf::from("insmaller")));
        assert_eq!(rv.get("self_exe").and_then(Value::as_str), Some("insmaller"));
        assert!(rv.get("exe_dir").is_none());
    }

    #[test]
    fn usage_string_uses_derived_program_name() {
        let u = usage_text("mytool");
        assert!(u.contains("mytool <key…>"));
        assert!(u.contains("mytool install"));
        assert!(u.contains("mytool task"));
        // Default name still works for direct usage.
        assert!(usage_text("insmaller").contains("insmaller install"));
    }

    #[test]
    fn posix_app_home_xdg_when_set() {
        // POSIX: $XDG_CONFIG_HOME/<name>/installer.toml comes first.
        let cands = app_home_candidates_posix(
            "mytool",
            Some("/tmp/xdg"),
            Some(Path::new("/home/u/.config")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(cands[0], PathBuf::from("/tmp/xdg/mytool/installer.toml"));
        assert_eq!(cands[1], PathBuf::from("/home/u/.mytool/installer.toml"));
        assert_eq!(cands[2], PathBuf::from("/etc/mytool/installer.toml"));
    }

    #[test]
    fn posix_app_home_config_dir_fallback_when_xdg_unset() {
        let cands = app_home_candidates_posix(
            "mytool",
            None,
            Some(Path::new("/home/u/.config")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            cands[0],
            PathBuf::from("/home/u/.config/mytool/installer.toml")
        );
    }

    #[test]
    fn posix_app_home_empty_xdg_treated_as_unset() {
        // `XDG_CONFIG_HOME=` (empty) must fall back to config_dir, not produce
        // a relative `mytool/installer.toml`.
        let cands = app_home_candidates_posix(
            "mytool",
            Some(""),
            Some(Path::new("/home/u/.config")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            cands[0],
            PathBuf::from("/home/u/.config/mytool/installer.toml")
        );
        // Not the bogus relative candidate that an unfiltered empty var produces.
        assert_ne!(cands[0], PathBuf::from("mytool").join("installer.toml"));
    }

    #[test]
    fn windows_app_home_appdata_when_set() {
        let cands = app_home_candidates_windows(
            "mytool",
            Some(r"C:\Users\u\AppData\Roaming"),
            None,
            Some(Path::new(r"C:\Users\u")),
            Some(r"C:\ProgramData"),
            None,
        );
        // Build expected with the same `join` the fn uses, so the assert holds
        // on every platform (a backslash literal is one component on Unix).
        assert_eq!(
            cands[0],
            PathBuf::from(r"C:\Users\u\AppData\Roaming").join("mytool").join("installer.toml")
        );
        assert_eq!(
            cands[1],
            PathBuf::from(r"C:\Users\u").join(".mytool").join("installer.toml")
        );
        assert_eq!(
            cands[2],
            PathBuf::from(r"C:\ProgramData").join("mytool").join("installer.toml")
        );
    }

    #[test]
    fn windows_app_home_empty_env_treated_as_unset() {
        // Empty `%APPDATA%` and `%PROGRAMDATA%` fall back to config_dir/data_dir
        // rather than producing relative candidates.
        let cands = app_home_candidates_windows(
            "mytool",
            Some(""),
            Some(Path::new(r"C:\Users\u\AppData\Roaming")),
            Some(Path::new(r"C:\Users\u")),
            Some(""),
            Some(Path::new(r"C:\ProgramData")),
        );
        assert_eq!(
            cands[0],
            PathBuf::from(r"C:\Users\u\AppData\Roaming").join("mytool").join("installer.toml")
        );
        assert_eq!(
            cands[2],
            PathBuf::from(r"C:\ProgramData").join("mytool").join("installer.toml")
        );
    }

    #[test]
    fn windows_app_home_config_dir_fallback_when_appdata_unset() {
        let cands = app_home_candidates_windows(
            "mytool",
            None,
            Some(Path::new(r"C:\Users\u\AppData\Roaming")),
            Some(Path::new(r"C:\Users\u")),
            None,
            Some(Path::new(r"C:\ProgramData")),
        );
        assert_eq!(
            cands[0],
            PathBuf::from(r"C:\Users\u\AppData\Roaming").join("mytool").join("installer.toml")
        );
        assert_eq!(
            cands[2],
            PathBuf::from(r"C:\ProgramData").join("mytool").join("installer.toml")
        );
    }

    // ── format_answer_value (answers masking) ─────────────────────────────

    #[test]
    fn secret_field_is_masked() {
        let v = Value::String("sk-supersecretkey".to_string());
        assert_eq!(format_answer_value(&v, true), "***");
    }

    #[test]
    fn string_field_shown_plaintext() {
        let v = Value::String("PH".to_string());
        assert_eq!(format_answer_value(&v, false), "PH");
    }

    #[test]
    fn array_value_joined_with_comma() {
        let v = Value::Array(vec![
            Value::String("node".to_string()),
            Value::String("ripgrep".to_string()),
        ]);
        assert_eq!(format_answer_value(&v, false), "node, ripgrep");
    }

    #[test]
    fn bool_value_rendered_as_string() {
        assert_eq!(format_answer_value(&Value::Bool(true), false), "true");
        assert_eq!(format_answer_value(&Value::Bool(false), false), "false");
    }

    #[test]
    fn secret_array_is_still_masked() {
        // Even an array in a secret field is fully masked.
        let v = Value::Array(vec![Value::String("tok1".to_string())]);
        assert_eq!(format_answer_value(&v, true), "***");
    }

    // ── requires_input secret masking ─────────────────────────────────────

    #[test]
    fn requires_input_secret_is_masked() {
        // Build a catalog with a CLI entry whose requires_input declares a
        // Secret token. Confirm that required_inputs returns it and that the
        // token's value is masked (is_secret = true).
        let cat = insmaller_core::Catalog::from_json_str(
            r#"{"clis":[{
                "key":"myapp","install":"npm:myapp",
                "requires_input":[{"id":"MYAPP_TOKEN","type":"secret","required":true}]
            }]}"#,
        )
        .unwrap();

        let selected = vec!["myapp".to_string()];
        let decls = cat.required_inputs(&selected);
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].id, "MYAPP_TOKEN");
        assert_eq!(decls[0].r#type, insmaller_core::FieldType::Secret);

        // Simulate what cmd_setup does: check r#type == Secret → add to set.
        let mut secret_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for decl in &decls {
            if decl.r#type == insmaller_core::FieldType::Secret {
                secret_ids.insert(decl.id.clone());
            }
        }
        assert!(secret_ids.contains("MYAPP_TOKEN"),
            "requires_input secret must be in secret_ids");

        // Confirm the value is masked via format_answer_value.
        let token_val = Value::String("sk-supersecret".to_string());
        let is_secret = secret_ids.contains("MYAPP_TOKEN");
        assert_eq!(format_answer_value(&token_val, is_secret), "***",
            "requires_input secret value must be masked, not printed");
    }

    #[test]
    fn requires_input_non_secret_is_not_masked() {
        // A requires_input of type "text" must NOT be in secret_ids.
        let cat = insmaller_core::Catalog::from_json_str(
            r#"{"clis":[{
                "key":"tool","install":"npm:tool",
                "requires_input":[{"id":"AUTHOR","type":"text","required":true}]
            }]}"#,
        )
        .unwrap();
        let decls = cat.required_inputs(&["tool".to_string()]);
        let mut secret_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for decl in &decls {
            if decl.r#type == insmaller_core::FieldType::Secret {
                secret_ids.insert(decl.id.clone());
            }
        }
        assert!(!secret_ids.contains("AUTHOR"),
            "non-secret requires_input must not be masked");
        let val = Value::String("Alice".to_string());
        assert_eq!(format_answer_value(&val, false), "Alice");
    }
}
