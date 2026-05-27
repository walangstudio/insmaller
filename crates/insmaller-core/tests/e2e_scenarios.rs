//! Gap-hunting e2e: different configs exercising integration paths the unit
//! suite doesn't — register_as flow through a recipe, the prompt/Skip
//! keystone, fail-fast non-block, the retry loop, dry-run through the real
//! installer.toml + recipe-pack plugins, and real `exec`. Cross-platform
//! (no bash assumption): save_input (pure fs), `cargo --version`, dry-run.

use insmaller_core::{
    builtins, install_many, install_many_with, Catalog, EnvResolver, LoadedConfig,
    NullReporter, Reporter, RunOpts, Sentinel,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Records every reporter event so tests can assert step outcomes/logs.
#[derive(Clone, Default)]
struct VecRep(Arc<Mutex<Vec<String>>>);
impl Reporter for VecRep {
    fn step_start(&self, k: &str, s: &str) {
        self.0.lock().unwrap().push(format!("start:{k}:{s}"));
    }
    fn step_end(&self, k: &str, s: &str, ok: bool) {
        self.0.lock().unwrap().push(format!("end:{k}:{s}:{ok}"));
    }
    fn log(&self, m: &str) {
        self.0.lock().unwrap().push(format!("log:{m}"));
    }
}
impl VecRep {
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn p(s: &std::path::Path) -> String {
    s.to_string_lossy().replace('\\', "/")
}

// A — register_as/locals flow through a recipe end-to-end.
#[tokio::test]
async fn register_flow_through_recipe() {
    let dir = tempfile::tempdir().unwrap();
    let (t1, t2) = (dir.path().join("a.env"), dir.path().join("b.env"));
    let cfg = LoadedConfig::from_str(&format!(
        r#"
        [[desugar]]
        prefix = "a:"
        recipe = "chain"
        parse  = "single_arg"
        [[recipe]]
        name = "chain"
        [[recipe.install]]
        type = "save_input"
        name = "GREET"
        value = "hi-{{{{ arg }}}}"
        file = "{}"
        [[recipe.install]]
        type = "save_input"
        name = "ECHO"
        value = "{{{{ GREET }}}}"
        file = "{}"
        "#,
        p(&t1),
        p(&t2)
    ))
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"a:foo"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many(&cat, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["c".into()]).await;
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert!(std::fs::read_to_string(&t1).unwrap().contains("GREET=hi-foo"));
    // Proves GREET registered by step 1 was visible to step 2's template.
    assert!(std::fs::read_to_string(&t2).unwrap().contains("ECHO=hi-foo"));
}

// B — keystone: optional prompt → Skip → dependent step skipped (not failed),
// and the skipped step reports ok (the step_end gap fix).
#[tokio::test]
async fn optional_prompt_skip_chain_and_reporting() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("should_not_exist.env");
    let cfg = LoadedConfig::from_str(&format!(
        r#"
        [[desugar]]
        prefix = "b:"
        recipe = "opt"
        parse  = "single_arg"
        [[recipe]]
        name = "opt"
        [[recipe.install]]
        type = "prompt"
        name = "INSM_OPT_UNSET_XYZ"
        required = false
        [[recipe.install]]
        type = "save_input"
        name = "USED"
        requires = ["INSM_OPT_UNSET_XYZ"]
        value = "nope"
        file = "{}"
        "#,
        p(&out)
    ))
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"b:x"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let rep = VecRep::default();
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many(&cat, &cfg, &reg, &rep, &EnvResolver, &sent, &["c".into()]).await;
    assert_eq!(s.completed, vec!["c"]);
    assert!(!out.exists(), "dependent step must be skipped");
    let ev = rep.events();
    // The skipped prompt must NOT be reported as a failure.
    assert!(
        ev.iter().any(|e| e == "end:c:prompt:true"),
        "skipped prompt should report ok=true, got {ev:?}"
    );
    assert!(!ev.iter().any(|e| e.contains("prompt:false")));
}

// C — required prompt missing under EnvResolver: fail fast, never block.
#[tokio::test]
async fn required_prompt_missing_fails_fast_without_blocking() {
    let cfg = LoadedConfig::from_str(
        r#"
        [[desugar]]
        prefix = "c:"
        recipe = "need"
        parse  = "single_arg"
        [[recipe]]
        name = "need"
        [[recipe.install]]
        type = "prompt"
        name = "INSM_MUSTHAVE_UNSET_XYZ"
        required = true
        "#,
    )
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"c:x"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let rep = NullReporter;
    let inp = EnvResolver;
    let keys = vec!["c".to_string()];
    let fut = install_many(&cat, &cfg, &reg, &rep, &inp, &sent, &keys);
    let s = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .expect("must not block on a required prompt under EnvResolver");
    assert_eq!(s.failed.len(), 1);
    assert!(s.failed[0].1.contains("INSM_MUSTHAVE_UNSET_XYZ"));
    assert!(!sent.is_installed("cli", "c"));
}

// D — retry loop actually re-invokes (download to a dead local port fails
// fast, no external network needed).
#[tokio::test]
async fn retry_loop_reinvokes_then_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = LoadedConfig::from_str(&format!(
        r#"
        [[desugar]]
        prefix = "d:"
        recipe = "dl"
        parse  = "single_arg"
        [[recipe]]
        name = "dl"
        [[recipe.install]]
        type = "download"
        url = "http://127.0.0.1:9/nope"
        dest = "{}/x"
        retries = 2
        "#,
        p(dir.path())
    ))
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"d:x"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let rep = VecRep::default();
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many(&cat, &cfg, &reg, &rep, &EnvResolver, &sent, &["c".into()]).await;
    assert_eq!(s.failed.len(), 1);
    let retries = rep
        .events()
        .iter()
        .filter(|e| e.contains("retry 1/2") || e.contains("retry 2/2"))
        .count();
    assert_eq!(retries, 2, "expected 2 retry logs, events: {:?}", rep.events());
}

// E — dry-run through the REAL installer.toml + lang-pkg plugin +
// VersionedPkg + a dependency: resolves the whole pipeline, spawns nothing,
// persists nothing.
#[tokio::test]
async fn dry_run_resolves_real_plugin_pipeline_without_side_effects() {
    let toml = concat!(env!("CARGO_MANIFEST_DIR"), "/../../installer.toml");
    let cfg = LoadedConfig::from_path(Path::new(toml)).unwrap();
    let cat = Catalog::from_json_str(
        r#"{ "tools":[
            {"key":"meta"},
            {"key":"rg","install":"cargo:ripgrep@14.1.0","dependencies":["meta"]}
        ]}"#,
    )
    .unwrap();
    let reg = builtins(&cfg.settings);
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many_with(
        &cat,
        &cfg,
        &reg,
        &NullReporter,
        &EnvResolver,
        &sent,
        &["rg".into()],
        RunOpts { dry_run: true, ..Default::default() },
        None,
    )
    .await;
    assert!(s.failed.is_empty(), "{:?}", s.failed); // cargo: desugared via lang-pkg
    assert_eq!(s.completed, vec!["rg"]);
    assert!(!sent.is_installed("tools", "rg")); // dry-run persists nothing
    assert!(!sent.is_installed("tools", "meta"));
}

// F — real exec end-to-end: resolve_in_path + enriched PATH + run_cmd +
// sentinel + idempotent rerun. `cargo` is on PATH in any Rust env.
#[tokio::test]
async fn real_exec_runs_and_is_idempotent() {
    let cfg = LoadedConfig::from_str(
        r#"
        [[desugar]]
        prefix = "ver:"
        recipe = "showver"
        parse  = "single_arg"
        [[recipe]]
        name = "showver"
        [[recipe.install]]
        type = "exec"
        program = "cargo"
        argline = "--version"
        "#,
    )
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"ver:x"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let rep = VecRep::default();
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many(&cat, &cfg, &reg, &rep, &EnvResolver, &sent, &["c".into()]).await;
    assert!(s.failed.is_empty(), "exec cargo --version: {:?}", s.failed);
    assert!(sent.is_installed("cli", "c"));
    let first = rep.events().len();
    // Rerun: sentinel short-circuits, exec not invoked again.
    let s2 = install_many(&cat, &cfg, &reg, &rep, &EnvResolver, &sent, &["c".into()]).await;
    assert_eq!(s2.completed, vec!["c"]);
    assert_eq!(rep.events().len(), first, "idempotent rerun ran no steps");
}

// H — the real sibling catalog (inline generic steps) loads against the real
// installer.toml and resolves end-to-end under dry-run (no network/builds):
// proves insmaller can drive the F:/opt/projs/ai/claude apps as-is.
#[tokio::test]
async fn real_sibling_catalog_loads_and_resolves() {
    let toml = concat!(env!("CARGO_MANIFEST_DIR"), "/../../installer.toml");
    let catf = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/siblings.catalog.json");
    let cfg = LoadedConfig::from_path(Path::new(toml)).unwrap();
    let cat = Catalog::from_json_str(&std::fs::read_to_string(catf).unwrap())
        .expect("sibling catalog must parse + all inline steps valid");
    let reg = builtins(&insmaller_core::Settings::default());
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    // Deps (uv/node/cargo) are the reference installer recipes in installer.toml; under
    // dry-run nothing spawns and the whole graph resolves.
    let keys: Vec<String> = ["mememo", "chatgipite", "gd-skills", "borch", "pair-pressure"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let s = install_many_with(
        &cat,
        &cfg,
        &reg,
        &NullReporter,
        &EnvResolver,
        &sent,
        &keys,
        RunOpts { dry_run: true, ..Default::default() },
        None,
    )
    .await;
    assert!(s.failed.is_empty(), "sibling resolve failed: {:?}", s.failed);
    assert_eq!(s.completed.len(), 5);
}

// G — `dir` on a shell step actually sets the working directory
// (cross-platform: `echo` redirect works in bash and powershell).
#[tokio::test]
async fn shell_step_runs_in_dir() {
    let work = tempfile::tempdir().unwrap();
    let wp = p(work.path());
    let cfg = LoadedConfig::from_str(&format!(
        r#"
        [[desugar]]
        prefix = "d:"
        recipe = "ind"
        parse  = "single_arg"
        [[recipe]]
        name = "ind"
        [[recipe.install]]
        type = "shell"
        dir = "{wp}"
        script = "echo hi > marker.txt"
        "#
    ))
    .unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"c","install":"d:x"}]}"#).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many(&cat, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["c".into()])
        .await;
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    // The marker landed in the configured `dir`, not the process cwd.
    assert!(
        work.path().join("marker.txt").is_file(),
        "shell `dir` must set cwd"
    );
}
