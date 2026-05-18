//! The real sample wizard against the real sibling catalog: parses, honors
//! conditions, emits the right catalog keys, and the selection feeds
//! install_many under dry-run (proves the wizard→install path end-to-end,
//! non-blocking via StaticAnswerer).
use insmaller_core::{
    builtins, install_many_with, run_wizard, Catalog, EnvResolver, LoadedConfig, NullReporter,
    RunOpts, Sentinel, StaticAnswerer, WizardDef,
};
use serde_json::{json, Map};
use std::path::Path;

fn paths() -> (String, String, String) {
    let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    (
        format!("{base}/installer.toml"),
        format!("{base}/examples/siblings.catalog.json"),
        format!("{base}/examples/sample.wizard.toml"),
    )
}

#[tokio::test]
async fn sample_wizard_resolves_and_drives_install() {
    let (cfg_p, cat_p, wiz_p) = paths();
    let cfg = LoadedConfig::from_path(Path::new(&cfg_p)).unwrap();
    let cat = Catalog::from_json_str(&std::fs::read_to_string(&cat_p).unwrap()).unwrap();
    let wiz = WizardDef::from_str(&std::fs::read_to_string(&wiz_p).unwrap())
        .expect("sample wizard must parse");

    // Non-interactive answers: pick a tool that gates the keys page.
    let mut ans = Map::new();
    ans.insert("CODING_CLI".into(), json!("")); // no cli entries in this catalog
    ans.insert("INSTALL_TOOLS".into(), json!(["mememo", "pair-pressure"]));
    ans.insert("PP_AUTHOR".into(), json!("alice")); // required by gated page
    // GIT_AUTHOR_EMAIL omitted → optional → Skip (no error)

    let outcome = run_wizard(&wiz, &cat, &StaticAnswerer(ans), &[]).unwrap();
    assert_eq!(outcome.selected_keys, vec!["mememo", "pair-pressure"]);
    assert_eq!(outcome.vars.get("PP_AUTHOR").unwrap(), "alice");
    assert!(outcome.vars.get("GIT_AUTHOR_EMAIL").is_none());

    // The selection drives the engine (dry-run: resolves graph, no spawn).
    let reg = builtins(&insmaller_core::Settings::default());
    let sd = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(sd.path().into());
    let s = install_many_with(
        &cat,
        &cfg,
        &reg,
        &NullReporter,
        &EnvResolver,
        &sent,
        &outcome.selected_keys,
        RunOpts { dry_run: true, ..Default::default() },
        None,
    )
    .await;
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.completed.len(), 2);
}

#[tokio::test]
async fn gated_required_field_not_asked_when_condition_false() {
    let (_c, cat_p, wiz_p) = paths();
    let cat = Catalog::from_json_str(&std::fs::read_to_string(&cat_p).unwrap()).unwrap();
    let wiz = WizardDef::from_str(&std::fs::read_to_string(&wiz_p).unwrap()).unwrap();
    // Do NOT pick pair-pressure → the keys page (PP_AUTHOR required) is
    // skipped, so a missing PP_AUTHOR is not an error.
    let mut ans = Map::new();
    ans.insert("CODING_CLI".into(), json!(""));
    ans.insert("INSTALL_TOOLS".into(), json!(["mememo"]));
    let o = run_wizard(&wiz, &cat, &StaticAnswerer(ans), &[]).unwrap();
    assert_eq!(o.selected_keys, vec!["mememo"]);
    assert!(o.vars.get("PP_AUTHOR").is_none());
}
