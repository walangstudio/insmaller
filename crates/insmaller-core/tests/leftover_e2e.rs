//! End-to-end (offline, tempdir): the v0.3.0 leftover primitives —
//! `backup` + `merge_toml` + `merge_yaml` driven through the task runner with
//! a plain-command JSON patch, and `Sentinel::resolve` scope precedence. No
//! consumer-specific engine code; no Docker concepts.

use insmaller_core::{
    builtins, EnvResolver, LoadedConfig, NullReporter, Sentinel, SentinelScope, Settings,
};
use serde_json::{Map, Value};
use std::path::Path;

fn base() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string()
}

#[tokio::test]
async fn merge_and_backup_pipeline_runs_offline() {
    if std::env::consts::OS == "windows" {
        return; // fixture command bodies are POSIX (printf)
    }
    let b = base();
    let cfg = LoadedConfig::from_path(Path::new(&format!("{b}/examples/e2e-merge.toml"))).unwrap();
    let reg = builtins(&cfg.settings);

    let dir = tempfile::tempdir().unwrap();
    let toml_cfg = dir.path().join("config.toml");
    let yaml_cfg = dir.path().join("providers.yaml");
    std::fs::write(&toml_cfg, "[tool]\nname = \"old\"\nkeep = 1\n").unwrap();
    std::fs::write(&yaml_cfg, "provider:\n  model: a\n  keep: 1\n").unwrap();

    let mut run_vars: Map<String, Value> = Map::new();
    run_vars.insert(
        "TOML_CFG".into(),
        Value::String(toml_cfg.to_string_lossy().into_owned()),
    );
    run_vars.insert(
        "YAML_CFG".into(),
        Value::String(yaml_cfg.to_string_lossy().into_owned()),
    );

    insmaller_core::run_task("seed-toml", &cfg, &reg, &NullReporter, &EnvResolver, &run_vars)
        .await
        .expect("backup + merge_toml pipeline");
    insmaller_core::run_task("seed-yaml", &cfg, &reg, &NullReporter, &EnvResolver, &run_vars)
        .await
        .expect("merge_yaml");

    // backup produced exactly one timestamped sibling, byte-identical to the
    // pre-merge content.
    let baks: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
        .collect();
    assert_eq!(baks.len(), 1, "exactly one backup");
    assert_eq!(
        std::fs::read_to_string(baks[0].path()).unwrap(),
        "[tool]\nname = \"old\"\nkeep = 1\n"
    );

    // merge_toml deep-merged and preserved untouched keys.
    let merged: serde_json::Value =
        toml::from_str(&std::fs::read_to_string(&toml_cfg).unwrap()).unwrap();
    assert_eq!(
        merged,
        serde_json::json!({"tool":{"name":"old","keep":1,"added":true}})
    );

    // merge_yaml likewise.
    let y: serde_json::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&yaml_cfg).unwrap()).unwrap();
    assert_eq!(y, serde_json::json!({"provider":{"model":"b","keep":1}}));
}

#[test]
fn sentinel_scope_precedence_holds() {
    // default == historical global path
    let def = Settings::default();
    assert_eq!(
        Sentinel::resolve(&def, Some(Path::new("/proj"))).base(),
        Sentinel::new(&def.sentinel_dir_name).base()
    );
    // workspace anchors to the config dir
    let mut ws = Settings::default();
    ws.sentinel_scope = SentinelScope::Workspace;
    assert!(Sentinel::resolve(&ws, Some(Path::new("/proj")))
        .base()
        .ends_with("/proj/.insmaller"));
    // explicit path wins over scope
    let mut ex = ws.clone();
    ex.sentinel_path = Some("/explicit".into());
    assert_eq!(
        Sentinel::resolve(&ex, Some(Path::new("/proj"))).base(),
        Path::new("/explicit")
    );
}
