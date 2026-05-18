//! End-to-end (offline, StaticAnswerer): the codetainyrrr fixture catalog
//! with requires_input + entry condition + setup_output + project intro/outro,
//! loaded via the field mapping with no consumer-specific engine code.
//! Mirrors acceptance criterion #4.

use insmaller_core::{
    builtins, install_many_with, run_wizard, write_setup_output, Catalog, Ctx, EnvResolver,
    LoadedConfig, NullReporter, RunOpts, Sentinel, SetupOutput, StaticAnswerer, WizardDef,
};
use serde_json::{Map, Value};
use std::path::Path;

fn base() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string()
}

#[tokio::test]
async fn codetainyrrr_fixture_setup_and_dry_run_install() {
    let b = base();
    let cfg = LoadedConfig::from_path(Path::new(&format!("{b}/examples/e2e-installer.toml")))
        .expect("e2e installer config loads");
    let cat = Catalog::from_json_str(
        &std::fs::read_to_string(format!("{b}/examples/e2e-fixture.catalog.json")).unwrap(),
    )
    .expect("codetainyrrr-shape catalog loads with no consumer-specific code");
    let wiz = WizardDef::from_str(
        &std::fs::read_to_string(format!("{b}/examples/e2e-fixture.wizard.toml")).unwrap(),
    )
    .unwrap();

    // category alias → group; name passthrough.
    let alpha_opt = cat
        .options("cli")
        .into_iter()
        .find(|o| o.key == "alpha")
        .unwrap();
    assert_eq!(alpha_opt.group.as_deref(), Some("core"));
    assert_eq!(alpha_opt.name.as_deref(), Some("Alpha CLI"));

    // Unattended answers from the fixture file.
    let raw = std::fs::read_to_string(format!("{b}/examples/e2e-answers.toml")).unwrap();
    let ans: Map<String, Value> = serde_json::to_value(toml::from_str::<toml::Table>(&raw).unwrap())
        .unwrap()
        .as_object()
        .cloned()
        .unwrap();

    let proj = cfg.project.as_ref().unwrap();
    let outcome = run_wizard(&wiz, &cat, &StaticAnswerer(ans), &proj.group_order).unwrap();

    // alpha selected; ALPHA_TOKEN collected via selected.inputs (P1-A).
    assert_eq!(outcome.selected_keys, vec!["alpha"]);
    assert_eq!(outcome.vars.get("ALPHA_TOKEN").unwrap(), "test-secret");

    // intro/outro render through Ctx with project.extra + wizard vars (P2-A).
    let mut ic = Ctx::new();
    for (k, v) in &proj.extra {
        ic.set(k, v.as_str());
    }
    assert_eq!(ic.render(proj.intro_template.as_ref().unwrap()).unwrap(), "Welcome to E2E");
    let mut oc = Ctx::new();
    for (k, v) in &proj.extra {
        oc.set(k, v.as_str());
    }
    for (k, v) in &outcome.vars {
        if let Value::String(s) = v {
            oc.set(k, s.as_str());
        }
    }
    assert_eq!(
        oc.render(proj.outro_template.as_ref().unwrap()).unwrap(),
        "Done. token=test-secret img=e2e:1"
    );

    // setup_output sink (P1-C): redirect to a tempdir, assert exact body.
    let so_cfg = cfg.settings.setup_output.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("out.env");
    let so = SetupOutput {
        path: out_path.to_string_lossy().into_owned(),
        format: so_cfg.format,
        header: so_cfg.header.clone(),
        include: so_cfg.include.clone(),
        mode: None,
    };
    write_setup_output(&so, &outcome.vars).unwrap();
    assert_eq!(
        std::fs::read_to_string(&out_path).unwrap(),
        "# e2e test output\nALPHA_TOKEN=test-secret\n"
    );

    // Dry-run install of BOTH keys with the wizard vars as run_vars: beta's
    // entry condition `${OS_GATE} == 'linux'` is false (OS_GATE=macos) ⇒ beta
    // is skipped (counted completed, no sentinel), alpha resolves. Holds even
    // though keys are passed directly to install_many_with.
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
        &["alpha".into(), "beta".into()],
        RunOpts { dry_run: true, ..Default::default() },
        Some(&outcome.vars),
    )
    .await;
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.completed, vec!["alpha", "beta"]); // beta = skipped(condition)
    assert!(!sent.is_installed("cli", "beta"));
    assert!(!sent.is_installed("cli", "alpha")); // dry-run persists nothing
}

#[tokio::test]
async fn codetainyrrr_tasks_fixture_runs_per_p1d() {
    // P1-D acceptance #5: needs ordering, per-OS branch, poll, fail-fast,
    // project.extra templating — with zero Docker awareness in engine code.
    if std::env::consts::OS == "windows" {
        return; // fixture script bodies are POSIX (true/echo/exit)
    }
    let b = base();
    let cfg = LoadedConfig::from_path(Path::new(&format!("{b}/examples/e2e-tasks.toml"))).unwrap();
    let reg = builtins(&cfg.settings);
    let mut run_vars: Map<String, Value> = Map::new();
    for (k, v) in &cfg.project.as_ref().unwrap().extra {
        run_vars.insert(k.clone(), Value::String(v.clone()));
    }

    // deploy needs check-env first; both succeed.
    insmaller_core::run_task("deploy", &cfg, &reg, &NullReporter, &EnvResolver, &run_vars)
        .await
        .expect("deploy (needs check-env) runs in order");

    // poll-until-exit-zero succeeds on `true`.
    insmaller_core::run_task("wait-ready", &cfg, &reg, &NullReporter, &EnvResolver, &run_vars)
        .await
        .unwrap();

    // fail-early: first step exits non-zero ⇒ task fails fast.
    let r = insmaller_core::run_task(
        "fail-early",
        &cfg,
        &reg,
        &NullReporter,
        &EnvResolver,
        &run_vars,
    )
    .await;
    assert!(r.is_err());

    // missing task name → clear error.
    let m = insmaller_core::run_task("ghost", &cfg, &reg, &NullReporter, &EnvResolver, &run_vars)
        .await;
    assert!(format!("{}", m.unwrap_err()).contains("task 'ghost'"));
}
