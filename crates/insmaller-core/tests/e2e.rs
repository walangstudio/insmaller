//! End-to-end: JSON catalog → EntrySource → desugar → recipe → registry →
//! orchestrator → sentinel, exercising the real public API. Uses the
//! `sentinel_meta` processor so the whole pipeline runs with no subprocess /
//! network (real exec parity is the container's job per the strangler plan).

use insmaller_core::{
    builtins, install_many, Catalog, EnvResolver, LoadedConfig, NullReporter, Sentinel,
};

const ENGINE: &str = r#"
[[desugar]]
prefix = "noop:"
recipe = "noop"
parse  = "single_arg"

[[recipe]]
name = "noop"
[[recipe.install]]
type = "sentinel_meta"
"#;

const CATALOG: &str = r#"{
  "clis":   [{"key":"claude","install":"noop:claude"}],
  "tools":  [
     {"key":"node","install":"noop:node"},
     {"key":"ts","install":"noop:ts","dependencies":["node"]},
     {"key":"metaonly","dependencies":["node"]}
  ],
  "plugins":[{"key":"plug","install":"noop:plug","dependencies":["claude"]}]
}"#;

#[tokio::test]
async fn full_pipeline_installs_resolves_deps_and_is_idempotent() {
    let cfg = LoadedConfig::from_str(ENGINE).unwrap();
    let cat = Catalog::from_json_str(CATALOG).unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let dir = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(dir.path().to_path_buf());

    let keys = vec![
        "claude".to_string(),
        "ts".to_string(),
        "plug".to_string(),
        "metaonly".to_string(),
    ];
    let s = install_many(&cat, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &keys).await;

    assert!(s.failed.is_empty(), "unexpected failures: {:?}", s.failed);
    assert_eq!(s.completed.len(), 4);

    // Sentinels written under the right kinds.
    assert!(sent.is_installed("cli", "claude"));
    assert!(sent.is_installed("tools", "node")); // pulled in as a dep
    assert!(sent.is_installed("tools", "ts"));
    assert!(sent.is_installed("tools", "metaonly")); // spec-less meta entry
    assert!(sent.is_installed("plugins", "plug"));

    // Meta entry recorded with the "meta" spec sentinel.
    assert_eq!(sent.read("tools", "metaonly").unwrap().spec, "meta");
    assert_eq!(sent.read("tools", "ts").unwrap().spec, "noop:ts");

    // Idempotent: a second run is a no-op (sentinels already present).
    let s2 = install_many(&cat, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &keys).await;
    assert!(s2.failed.is_empty());
    assert_eq!(s2.completed.len(), 4);
}

#[tokio::test]
async fn unknown_spec_prefix_fails_that_key_only() {
    let cfg = LoadedConfig::from_str(ENGINE).unwrap();
    let cat = Catalog::from_json_str(
        r#"{ "tools":[
              {"key":"good","install":"noop:good"},
              {"key":"bad","install":"whoops:nothing"}
        ]}"#,
    )
    .unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let dir = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(dir.path().to_path_buf());

    let s = install_many(
        &cat,
        &cfg,
        &reg,
        &NullReporter,
        &EnvResolver,
        &sent,
        &["good".into(), "bad".into()],
    )
    .await;
    assert_eq!(s.completed, vec!["good"]);
    assert_eq!(s.failed.len(), 1);
    assert_eq!(s.failed[0].0, "bad");
    assert!(sent.is_installed("tools", "good"));
    assert!(!sent.is_installed("tools", "bad"));
}

/// Keystone: the unattended path (EnvResolver) must never block. There is no
/// prompt processor yet, but this asserts a full run under EnvResolver
/// completes promptly rather than hanging.
#[tokio::test]
async fn unattended_run_completes_without_blocking() {
    let cfg = LoadedConfig::from_str(ENGINE).unwrap();
    let cat = Catalog::from_json_str(r#"{"clis":[{"key":"claude","install":"noop:claude"}]}"#)
        .unwrap();
    let reg = builtins(&insmaller_core::Settings::default());
    let dir = tempfile::tempdir().unwrap();
    let sent = Sentinel::with_base(dir.path().to_path_buf());
    let rep = NullReporter;
    let inp = EnvResolver;
    let keys = vec!["claude".to_string()];

    let fut = install_many(&cat, &cfg, &reg, &rep, &inp, &sent, &keys);
    let s = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .expect("must not hang under EnvResolver");
    assert_eq!(s.completed, vec!["claude"]);
}
