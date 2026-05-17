//! Recipe-pack plugin merge: conflict + error rules, via temp dirs.
use insmaller_core::LoadedConfig;
use std::fs;

fn write(p: &std::path::Path, s: &str) {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(p, s).unwrap();
}

#[test]
fn plugin_recipe_resolves_through_namespaced_desugar() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("installer.toml"),
        r#"
        [[plugin]]
        name = "x"
        path = "plugins/x"
        "#,
    );
    write(
        &dir.path().join("plugins/x/installer.plugin.toml"),
        r#"
        [[desugar]]
        prefix = "foo:"
        recipe = "do"
        parse  = "single_arg"
        [[recipe]]
        name = "do"
        [[recipe.install]]
        type = "sentinel_meta"
        "#,
    );
    let cfg = LoadedConfig::from_path(&dir.path().join("installer.toml")).unwrap();
    assert!(cfg.recipe("x/do").is_some());
    let r = cfg.desugar.iter().find(|d| d.prefix == "foo:").unwrap();
    assert_eq!(r.recipe, "x/do"); // rewritten to namespaced
}

#[test]
fn two_plugins_claiming_same_prefix_is_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("installer.toml"),
        r#"
        [[plugin]]
        name = "a"
        path = "plugins/a"
        [[plugin]]
        name = "b"
        path = "plugins/b"
        "#,
    );
    for n in ["a", "b"] {
        write(
            &dir.path().join(format!("plugins/{n}/installer.plugin.toml")),
            r#"
            [[desugar]]
            prefix = "dup:"
            recipe = "r"
            parse  = "single_arg"
            [[recipe]]
            name = "r"
            [[recipe.install]]
            type = "sentinel_meta"
            "#,
        );
    }
    let err = LoadedConfig::from_path(&dir.path().join("installer.toml")).unwrap_err();
    assert!(format!("{err}").contains("dup:"));
}

#[test]
fn core_wins_prefix_collision_with_plugin() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("installer.toml"),
        r#"
        [[desugar]]
        prefix = "shared:"
        recipe = "core_r"
        parse  = "single_arg"
        [[recipe]]
        name = "core_r"
        [[recipe.install]]
        type = "sentinel_meta"

        [[plugin]]
        name = "p"
        path = "plugins/p"
        "#,
    );
    write(
        &dir.path().join("plugins/p/installer.plugin.toml"),
        r#"
        [[desugar]]
        prefix = "shared:"
        recipe = "plug_r"
        parse  = "single_arg"
        [[recipe]]
        name = "plug_r"
        [[recipe.install]]
        type = "sentinel_meta"
        "#,
    );
    let cfg = LoadedConfig::from_path(&dir.path().join("installer.toml")).unwrap();
    let shared: Vec<_> = cfg.desugar.iter().filter(|d| d.prefix == "shared:").collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].recipe, "core_r"); // core wins, plugin dropped
}

#[test]
fn duplicate_plugin_name_is_error() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("installer.toml"),
        r#"
        [[plugin]]
        name = "dup"
        path = "plugins/dup"
        [[plugin]]
        name = "dup"
        path = "plugins/dup"
        "#,
    );
    write(
        &dir.path().join("plugins/dup/installer.plugin.toml"),
        r#"
        [[recipe]]
        name = "r"
        [[recipe.install]]
        type = "sentinel_meta"
        "#,
    );
    assert!(LoadedConfig::from_path(&dir.path().join("installer.toml")).is_err());
}

#[test]
fn plugin_path_traversal_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let cfgdir = root.path().join("cfg");
    fs::create_dir_all(&cfgdir).unwrap();
    // A real plugin dir OUTSIDE the config dir → canonicalize succeeds, the
    // starts_with(base) bound check must reject it.
    write(
        &root.path().join("outside/installer.plugin.toml"),
        "[[recipe]]\nname=\"r\"\n[[recipe.install]]\ntype=\"sentinel_meta\"\n",
    );
    write(
        &cfgdir.join("installer.toml"),
        r#"
        [[plugin]]
        name = "bad"
        path = "../outside"
        "#,
    );
    let err = LoadedConfig::from_path(&cfgdir.join("installer.toml")).unwrap_err();
    assert!(
        format!("{err}").contains("escapes") || format!("{err}").contains("path"),
        "got: {err}"
    );
}

#[test]
fn from_str_keeps_transport_plugins() {
    // command/wasm/cdylib plugins need no base dir → retained by from_str so
    // register_external works (gap #6).
    let cfg = LoadedConfig::from_str(
        r#"
        [[plugin]]
        name = "p"
        command = "mycli --serve"
        kinds = ["custom"]
        "#,
    )
    .unwrap();
    assert_eq!(cfg.plugins.len(), 1);
    assert_eq!(cfg.plugins[0].command.as_deref(), Some("mycli --serve"));
}

#[test]
fn from_str_rejects_recipe_pack_plugin() {
    // path-based plugin needs from_path; from_str must refuse it.
    let err = LoadedConfig::from_str(
        r#"
        [[plugin]]
        name = "x"
        path = "plugins/x"
        "#,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("from_path"));
}
