//! Standalone harness for the engine.
//!
//!   insmaller <key…>            [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller install   <key…> [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller uninstall <key…> [--config F] [--catalog F] [--dry-run] [--json]
//!   insmaller setup [--wizard F] [--catalog F] [--config F] [--answers F] [--dry-run]
//!   insmaller status [<key>] [--config F] [--json]
//!
//! insmaller is an installer: a bare `insmaller <key…>` (no recognized
//! subcommand) defaults to `install`. `uninstall` runs each recipe's
//! `uninstall` phase and clears its sentinels. `setup` runs the pages/wizard,
//! then installs the selected keys (wizard string answers are exported to the
//! env so prompt/save_input/EnvResolver pick them up). `--answers` makes
//! `setup` fully unattended (non-blocking).

mod theme;
mod tui;

use insmaller_core::{
    builtins, run_wizard, Catalog, Ctx, EnvResolver, InstallSummary, LoadedConfig, Reporter,
    Sentinel, SentinelData, StaticAnswerer, StdoutReporter, WizardDef, WizardOutcome,
    WizardSession,
};
use serde_json::{Map, Value};
use std::io::IsTerminal;
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

/// `--config` if given, else the discovered config, else the legacy
/// `installer.toml` (so a missing-file error names something sensible).
fn discover_config(explicit: Option<String>) -> String {
    if let Some(e) = explicit {
        return e;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_config(&cwd, CONFIG_NAMES, |p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "installer.toml".to_string())
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

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => cmd_op(&args[1..], Op::Install).await,
        Some("uninstall") | Some("remove") => cmd_op(&args[1..], Op::Uninstall).await,
        Some("setup") => cmd_setup(&args[1..]).await,
        Some("task") | Some("run") => cmd_task(&args[1..]).await,
        Some("status") | Some("query") => cmd_status(&args[1..]).await,
        Some("-V") | Some("--version") | Some("version") => {
            println!("insmaller {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("-h") | Some("--help") | Some("help") | None => {
            eprintln!(
                "usage:\n  insmaller <key…>            [--config F] [--catalog F] [--dry-run] [--json]   (defaults to install)\n  insmaller install   <key…> [--config F] [--catalog F] [--dry-run] [--json]\n  insmaller uninstall <key…> [--config F] [--catalog F] [--dry-run] [--json] [--force]\n  insmaller setup [--wizard F] [--catalog F] [--config F] [--answers F] [--dry-run]\n  insmaller task <name…>     [--config F]   (alias: insmaller run <name…>)\n  insmaller status [<key>]   [--config F] [--json]   (alias: insmaller query)\n\n--config: if omitted, the first of insmaller.toml/.insmaller.toml/\ninstaller.toml found in the cwd or any parent dir.\n--catalog/--wizard default to the config's `[settings] catalog`/`wizard`\n(relative to the config file) if set, else catalog.json/wizard.toml in cwd.\n--force: uninstall even if another installed key still depends on it.\ntask: runs a `[task.<name>]` pipeline (needs first, per-OS, fail-fast)."
            );
            if args.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        // insmaller is an installer: anything else is treated as install keys.
        _ => cmd_op(&args, Op::Install).await,
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
) -> (InstallSummary, &'static str) {
    let mut reg = builtins(&cfg.settings);
    insmaller_core::register_external(&mut reg, &cfg.plugins);
    let sent = sentinel_for(cfg, cfg_p);
    let opts = insmaller_core::RunOpts { dry_run, force };
    match op {
        Op::Install => (
            insmaller_core::install_many_with(
                cat, cfg, &reg, rep, &EnvResolver, &sent, keys, opts, run_vars,
            )
            .await,
            "ok",
        ),
        Op::Uninstall => (
            insmaller_core::uninstall_many_with(
                cat, cfg, &reg, rep, &EnvResolver, &sent, keys, opts,
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
            "--config" | "--catalog" => i += 2,
            "--dry-run" | "--json" | "--force" => i += 1,
            k => {
                keys.push(k.to_string());
                i += 1;
            }
        }
    }
    keys
}

async fn cmd_op(a: &[String], op: Op) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"));
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

async fn cmd_setup(a: &[String]) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"));
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
    let unattended = has(a, "--answers") || !std::io::stdin().is_terminal();
    let (outcome, tui_used): (WizardOutcome, bool) = if unattended {
        let f = opt(a, "--answers", "answers.toml");
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        let m: Map<String, Value> = toml::from_str::<toml::Table>(&raw)
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_object().cloned())
            .or_else(|| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        match run_wizard(&wiz, &cat, &StaticAnswerer(m), &group_order) {
            Ok(o) => (o, false),
            Err(e) => {
                eprintln!("wizard error: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let mut session = WizardSession::new(&wiz, &cat, group_order.clone());
        match tui::run_wizard_tui(&mut session, palette) {
            Ok(true) => (session.finish(), true),
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
        if let Err(e) = insmaller_core::write_setup_output(so, &outcome.vars) {
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

    println!("Selected: {:?}", outcome.selected_keys);
    if outcome.selected_keys.is_empty() {
        println!("Nothing selected.");
        render_outro();
        return ExitCode::SUCCESS;
    }
    let dry_run = has(a, "--dry-run");
    if !tui_used {
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
        )
        .await;
        let code = summarize(&s, verb);
        render_outro();
        return code;
    }
    // Interactive: indicatif spinner for the install phase. The bar must be
    // cleared before the summary prints, hence run_op → finish → summarize.
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
    )
    .await;
    bar.finish();
    let code = summarize(&s, verb);
    render_outro();
    code
}

/// `insmaller task <name…>` / `insmaller run <name…>` — run named lifecycle
/// task pipelines. `run_vars` = project.extra + process env (so task scripts
/// template the consumer's image_tag/container_name/etc).
async fn cmd_task(a: &[String]) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"));
    let names = collect_keys(a);
    let cfg = match LoadedConfig::from_path(std::path::Path::new(&cfg_p)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if names.is_empty() {
        eprintln!("usage: insmaller task <name…>  (available: {:?})", cfg.tasks.keys().collect::<Vec<_>>());
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
    let mut reg = builtins(&cfg.settings);
    insmaller_core::register_external(&mut reg, &cfg.plugins);
    for name in &names {
        if let Err(e) =
            insmaller_core::run_task(name, &cfg, &reg, &StdoutReporter, &EnvResolver, &run_vars)
                .await
        {
            eprintln!("task '{name}' failed: {e:#}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// `insmaller status [<key>]` — read-only listing of what the scope-aware
/// sentinel records as installed. `--json` emits an array; otherwise an
/// aligned table. Optional positional filters to one key. Always SUCCESS
/// (empty is not an error); only a config load failure is FAILURE.
async fn cmd_status(a: &[String]) -> ExitCode {
    let cfg_p = discover_config(opt_opt(a, "--config"));
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

#[cfg(test)]
mod tests {
    use super::{find_config, resolve_sibling, CONFIG_NAMES};
    use std::path::{Path, PathBuf};

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
}
