//! Validates the repo's real installer.toml + its recipe-pack plugins: it
//! parses, every desugar rule resolves to a defined recipe, handler-equivalent
//! recipes are present, and plugins are namespaced / core wins collisions.
use insmaller_core::LoadedConfig;
use std::path::Path;

fn load() -> LoadedConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../installer.toml");
    LoadedConfig::from_path(Path::new(path)).expect("installer.toml must parse + cross-check")
}

#[test]
fn installer_toml_loads_and_cross_references() {
    let cfg = load();
    assert_eq!(cfg.settings.sentinel_dir_name, "codetainyrrr");
    assert!(cfg
        .settings
        .path_globs
        .iter()
        .any(|g| g.contains(".nvm/versions/node")));
}

#[test]
fn all_handler_equivalent_recipes_present() {
    let cfg = load();
    for r in [
        "npm-global",
        "apt-get",
        "uv-tool",
        "nvm-node",
        "sdkman-candidate",
        "corepack-prepare",
        "go-toolchain",
        "python-tools",
        "gh-release",
        "git-clone",
        "merge-json",
        "marketplace",
        "shell-pipe",
    ] {
        assert!(cfg.recipe(r).is_some(), "missing recipe {r}");
    }
    // python:tools uninstall is wired (codetainyrrr parity).
    assert!(!cfg.recipe("python-tools").unwrap().uninstall.is_empty());
}

#[test]
fn desugar_table_covers_every_spec_prefix() {
    let cfg = load();
    for p in [
        "npm:", "apt:", "uv:", "nvm:", "sdkman:", "corepack:", "go:", "python:", "gh:",
        "git:", "merge-json:", "marketplace:",
    ] {
        assert!(
            cfg.desugar.iter().any(|d| d.prefix == p),
            "no desugar rule for {p}"
        );
    }
}

#[test]
fn recipe_pack_plugins_merged_and_namespaced() {
    let cfg = load();
    // Plugin recipes are namespaced <plugin>/<recipe>.
    assert!(cfg.recipe("sys-pkg/apk").is_some());
    assert!(cfg.recipe("sys-pkg/winget").is_some());
    assert!(cfg.recipe("lang-pkg/pip").is_some());
    assert!(cfg.recipe("lang-pkg/cargo").is_some());
    // Bare (un-namespaced) plugin recipe names must NOT leak.
    assert!(cfg.recipe("apk").is_none());
    assert!(cfg.recipe("pip").is_none());

    // Plugin-owned prefixes route to the namespaced recipe.
    let cargo = cfg
        .desugar
        .iter()
        .find(|d| d.prefix == "cargo:")
        .expect("cargo: prefix");
    assert_eq!(cargo.recipe, "lang-pkg/cargo");

    // Breadth managers must exist as namespaced recipes and their prefixes
    // must route to them (catches recipe/desugar drift in the packs).
    for (prefix, recipe) in [
        ("rustup:", "lang-pkg/rustup"),
        ("asdf:", "lang-pkg/asdf"),
        ("mise:", "lang-pkg/mise"),
        ("composer:", "lang-pkg/composer"),
        ("deno:", "lang-pkg/deno"),
        ("bun:", "lang-pkg/bun"),
        ("zypper:", "sys-pkg/zypper"),
    ] {
        assert!(cfg.recipe(recipe).is_some(), "missing recipe {recipe}");
        let d = cfg
            .desugar
            .iter()
            .find(|d| d.prefix == prefix)
            .unwrap_or_else(|| panic!("missing {prefix} prefix"));
        assert_eq!(d.recipe, recipe, "{prefix} must route to {recipe}");
    }

    // Core wins the `apt:` prefix collision (sys-pkg also declares apt:):
    // exactly one apt: rule, pointing at the core recipe.
    let apt: Vec<_> = cfg.desugar.iter().filter(|d| d.prefix == "apt:").collect();
    assert_eq!(apt.len(), 1, "core must win, no duplicate apt:");
    assert_eq!(apt[0].recipe, "apt-get");
}
