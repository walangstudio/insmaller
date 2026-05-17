//! Terse-spec → recipe + params. Each parser RELOCATES the codetainyrrr
//! handler's parse logic verbatim (parity guarantee). Source handlers:
//! npm.rs, uv.rs, git_clone.rs, github_release.rs, nvm.rs, marketplace.rs,
//! merge_json.rs. Behavior — including bail conditions — is identical.

use crate::config::{LoadedConfig, ParseKind};
use crate::error::{EngineError, Result};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Desugared {
    pub recipe: String,
    pub params: Map<String, Value>,
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// Find the matching desugar rule (prefix rules first, in config order;
/// then a ShellLiteral catch-all), parse the remainder into params.
pub fn desugar(spec: &str, cfg: &LoadedConfig) -> Result<Desugared> {
    for rule in &cfg.desugar {
        if rule.parse == ParseKind::ShellLiteral {
            continue;
        }
        if !rule.prefix.is_empty() && spec.starts_with(&rule.prefix) {
            let params = parse(rule.parse, &rule.prefix, spec)?;
            return Ok(Desugared {
                recipe: rule.recipe.clone(),
                params,
            });
        }
    }
    // shell_literal is the catch-all ONLY for genuine shell pipelines
    // (codetainyrrr registry's real condition). A typo'd/unknown spec must
    // error loudly, not silently execute as a shell script.
    if cfg.settings.allow_shell_literal && looks_like_shell(spec) {
        if let Some(rule) = cfg.desugar.iter().find(|r| r.parse == ParseKind::ShellLiteral) {
            return Ok(Desugared {
                recipe: rule.recipe.clone(),
                params: parse(ParseKind::ShellLiteral, "", spec)?,
            });
        }
    }
    Err(EngineError::NoDesugar(spec.to_string()))
}

/// Mirrors codetainyrrr registry: a raw shell spec starts with a fetcher or
/// pipes into a shell. Anything else with no prefix rule is a config error.
fn looks_like_shell(spec: &str) -> bool {
    let s = spec.trim_start();
    s.starts_with("curl ")
        || s.starts_with("wget ")
        || s.starts_with("sh ")
        || s.starts_with("bash ")
        || s.contains("| bash")
        || s.contains("|bash")
        || s.contains("| sh")
        || s.contains("|sh")
}

fn parse(kind: ParseKind, prefix: &str, spec: &str) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    match kind {
        // npm.rs / apt.rs: whole remainder is the package list (handler
        // whitespace-splits at exec time; the exec processor mirrors that).
        ParseKind::RestVerbatim => {
            m.insert("packages".into(), s(spec.strip_prefix(prefix).unwrap_or(spec)));
        }
        // uv.rs::parse — verbatim.
        ParseKind::UvSpec => {
            let body = spec.strip_prefix(prefix).unwrap_or(spec);
            let (pkg, from) = match body.split_once('@') {
                Some((pkg, src)) if src.contains("://") || src.starts_with("git+") => {
                    (pkg, src)
                }
                _ => (body, ""),
            };
            m.insert("package".into(), s(pkg));
            m.insert("from".into(), s(from));
        }
        // git_clone.rs::split_url_dest — verbatim, including the bail.
        ParseKind::GitUrlDest => {
            let rest = spec.strip_prefix(prefix).unwrap_or(spec);
            let mut it = rest.rsplitn(2, ':');
            let dest = it.next().unwrap_or("");
            let url = it.next().unwrap_or("");
            let dest_looks_like_path = !dest.starts_with("//")
                && (dest.starts_with('/') || dest.starts_with('~') || dest.starts_with('$'));
            if url.is_empty() || dest.is_empty() || !dest_looks_like_path {
                return Err(EngineError::BadSpec(format!(
                    "git: spec must be git:<url>:<install_to> where install_to starts with /, ~, or $; got: {spec}"
                )));
            }
            m.insert("url".into(), s(url));
            m.insert("dest".into(), s(dest));
        }
        // github_release.rs parse + glob→ERE — verbatim.
        ParseKind::GhRepoAsset => {
            let rest = spec.strip_prefix(prefix).unwrap_or(spec);
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            if parts.len() != 2 {
                return Err(EngineError::BadSpec(format!(
                    "gh: spec must be gh:<owner/repo>:<asset_pattern>, got: {spec}"
                )));
            }
            let pattern = parts[1];
            let regex = pattern.replace('.', r"\.").replace('*', ".*");
            m.insert("repo".into(), s(parts[0]));
            m.insert("pattern".into(), s(pattern));
            m.insert("pattern_regex".into(), s(&regex));
        }
        // nvm.rs::{to_nvm_install_arg,to_nvm_alias_target} — verbatim.
        ParseKind::NvmVersion => {
            let version = spec.strip_prefix(prefix).unwrap_or("lts");
            let install_arg = match version {
                "lts" | "lts/*" => "--lts",
                other => other,
            };
            let alias_target = match version {
                "lts" => "lts/*",
                other => other,
            };
            m.insert("version".into(), s(version));
            m.insert("install_arg".into(), s(install_arg));
            m.insert("alias_target".into(), s(alias_target));
        }
        // marketplace.rs::parse_spec — verbatim.
        ParseKind::MarketplaceSpec => {
            let rest = spec.strip_prefix(prefix).unwrap_or(spec);
            let parts: Vec<&str> = rest.splitn(3, ':').collect();
            if parts.len() < 2 {
                return Err(EngineError::BadSpec(format!(
                    "invalid marketplace spec: {spec}"
                )));
            }
            let mkt = parts.get(2).copied().unwrap_or(parts[1]);
            m.insert("repo".into(), s(parts[0]));
            m.insert("plugin".into(), s(parts[1]));
            m.insert("marketplace".into(), s(mkt));
        }
        // merge_json.rs split — verbatim (path left raw; the processor
        // expand_home()s it, mirroring the handler's runtime expansion).
        ParseKind::MergeJsonSpec => {
            let rest = spec.strip_prefix(prefix).unwrap_or(spec);
            let (path, cmd) = rest.split_once(':').ok_or_else(|| {
                EngineError::BadSpec(format!(
                    "merge-json: spec must be merge-json:<path>:<cmd>, got: {spec}"
                ))
            })?;
            m.insert("target".into(), s(path));
            m.insert("command".into(), s(cmd));
        }
        // sdkman/corepack/go: single-token remainder.
        ParseKind::SingleArg => {
            m.insert("arg".into(), s(spec.strip_prefix(prefix).unwrap_or(spec)));
        }
        // pip/cargo/gem/…: `name[@version]` → name, version ("" if absent).
        ParseKind::VersionedPkg => {
            let body = spec.strip_prefix(prefix).unwrap_or(spec);
            let (name, version) = match body.split_once('@') {
                Some((n, v)) => (n, v),
                None => (body, ""),
            };
            m.insert("name".into(), s(name));
            m.insert("version".into(), s(version));
        }
        // raw `curl … | bash` — whole spec is the script.
        ParseKind::ShellLiteral => {
            m.insert("script".into(), s(spec));
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedConfig;

    fn pmap(kind: ParseKind, prefix: &str, spec: &str) -> Map<String, Value> {
        parse(kind, prefix, spec).unwrap()
    }
    fn g<'a>(m: &'a Map<String, Value>, k: &str) -> &'a str {
        m.get(k).unwrap().as_str().unwrap()
    }

    // ── ported from codetainyrrr uv.rs::tests ──────────────────────────────
    #[test]
    fn uv_plain_package() {
        let m = pmap(ParseKind::UvSpec, "uv:", "uv:aider-chat");
        assert_eq!(g(&m, "package"), "aider-chat");
        assert_eq!(g(&m, "from"), "");
    }
    #[test]
    fn uv_versioned_pep440_is_not_from() {
        let m = pmap(ParseKind::UvSpec, "uv:", "uv:poetry@1.7");
        assert_eq!(g(&m, "package"), "poetry@1.7");
        assert_eq!(g(&m, "from"), "");
    }
    #[test]
    fn uv_git_from_url() {
        let m = pmap(
            ParseKind::UvSpec,
            "uv:",
            "uv:specify-cli@git+https://github.com/github/spec-kit.git",
        );
        assert_eq!(g(&m, "package"), "specify-cli");
        assert_eq!(g(&m, "from"), "git+https://github.com/github/spec-kit.git");
    }
    #[test]
    fn uv_https_from_url() {
        let m = pmap(
            ParseKind::UvSpec,
            "uv:",
            "uv:foo@https://example.com/foo.tar.gz",
        );
        assert_eq!(g(&m, "package"), "foo");
        assert_eq!(g(&m, "from"), "https://example.com/foo.tar.gz");
    }

    // ── ported from codetainyrrr git_clone.rs::tests ───────────────────────
    #[test]
    fn git_https_url_with_colon_splits_at_last_colon() {
        let m = pmap(
            ParseKind::GitUrlDest,
            "git:",
            "git:https://github.com/flutter/flutter.git:$HOME/.flutter",
        );
        assert_eq!(g(&m, "url"), "https://github.com/flutter/flutter.git");
        assert_eq!(g(&m, "dest"), "$HOME/.flutter");
    }
    #[test]
    fn git_ssh_url_splits_correctly() {
        let m = pmap(
            ParseKind::GitUrlDest,
            "git:",
            "git:git@github.com:user/repo.git:~/repo",
        );
        assert_eq!(g(&m, "url"), "git@github.com:user/repo.git");
        assert_eq!(g(&m, "dest"), "~/repo");
    }
    #[test]
    fn git_no_colon_at_all_errors() {
        assert!(parse(ParseKind::GitUrlDest, "git:", "git:malformed-no-colons").is_err());
    }
    #[test]
    fn git_dest_not_anchored_errors() {
        assert!(parse(ParseKind::GitUrlDest, "git:", "git:https://example.com/repo.git").is_err());
    }

    // ── github_release glob→ERE ────────────────────────────────────────────
    #[test]
    fn gh_pattern_regex_quotes_dots_expands_stars() {
        let m = pmap(
            ParseKind::GhRepoAsset,
            "gh:",
            "gh:jesseduffield/lazygit:*Linux_x86_64*.tar.gz",
        );
        assert_eq!(g(&m, "repo"), "jesseduffield/lazygit");
        assert_eq!(g(&m, "pattern"), "*Linux_x86_64*.tar.gz");
        assert_eq!(g(&m, "pattern_regex"), r".*Linux_x86_64.*\.tar\.gz");
    }
    #[test]
    fn gh_missing_pattern_errors() {
        assert!(parse(ParseKind::GhRepoAsset, "gh:", "gh:owner/repo").is_err());
    }

    // ── nvm version translation ────────────────────────────────────────────
    #[test]
    fn nvm_lts_maps_to_flags() {
        let m = pmap(ParseKind::NvmVersion, "nvm:", "nvm:lts");
        assert_eq!(g(&m, "install_arg"), "--lts");
        assert_eq!(g(&m, "alias_target"), "lts/*");
    }
    #[test]
    fn nvm_concrete_version_passes_through() {
        let m = pmap(ParseKind::NvmVersion, "nvm:", "nvm:20.1.0");
        assert_eq!(g(&m, "install_arg"), "20.1.0");
        assert_eq!(g(&m, "alias_target"), "20.1.0");
    }

    // ── marketplace ────────────────────────────────────────────────────────
    #[test]
    fn marketplace_defaults_mkt_to_plugin() {
        let m = pmap(
            ParseKind::MarketplaceSpec,
            "marketplace:",
            "marketplace:github/spec-kit:spec-kit",
        );
        assert_eq!(g(&m, "repo"), "github/spec-kit");
        assert_eq!(g(&m, "plugin"), "spec-kit");
        assert_eq!(g(&m, "marketplace"), "spec-kit");
    }
    #[test]
    fn marketplace_explicit_mkt() {
        let m = pmap(
            ParseKind::MarketplaceSpec,
            "marketplace:",
            "marketplace:o/r:plug:mkt",
        );
        assert_eq!(g(&m, "marketplace"), "mkt");
    }
    #[test]
    fn marketplace_too_few_parts_errors() {
        assert!(parse(ParseKind::MarketplaceSpec, "marketplace:", "marketplace:onlyrepo").is_err());
    }

    // ── merge-json ─────────────────────────────────────────────────────────
    #[test]
    fn merge_json_splits_path_and_cmd_on_first_colon() {
        let m = pmap(
            ParseKind::MergeJsonSpec,
            "merge-json:",
            "merge-json:~/.claude/settings.json:echo '{\"a\":1}'",
        );
        assert_eq!(g(&m, "target"), "~/.claude/settings.json");
        assert_eq!(g(&m, "command"), "echo '{\"a\":1}'");
    }

    #[test]
    fn versioned_pkg_splits_optional_version() {
        let m = pmap(ParseKind::VersionedPkg, "pip:", "pip:black@24.3.0");
        assert_eq!(g(&m, "name"), "black");
        assert_eq!(g(&m, "version"), "24.3.0");
        let m2 = pmap(ParseKind::VersionedPkg, "cargo:", "cargo:ripgrep");
        assert_eq!(g(&m2, "name"), "ripgrep");
        assert_eq!(g(&m2, "version"), "");
    }

    // ── dispatch via LoadedConfig ──────────────────────────────────────────
    fn cfg() -> LoadedConfig {
        LoadedConfig::from_str(
            r#"
            [[desugar]]
            prefix = "npm:"
            recipe = "npm-global"
            parse = "rest_verbatim"

            [[desugar]]
            prefix = ""
            recipe = "shell-pipe"
            parse = "shell_literal"

            [[recipe]]
            name = "npm-global"
            [[recipe.install]]
            type = "exec"
            program = "npm"
            argline = "install -g {{ packages }}"

            [[recipe]]
            name = "shell-pipe"
            [[recipe.install]]
            type = "shell"
            script = "{{ script }}"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn allow_shell_literal_false_disables_the_catch_all() {
        let cfg = LoadedConfig::from_str(
            r#"
            [settings]
            allow_shell_literal = false

            [[desugar]]
            prefix = ""
            recipe = "shell-pipe"
            parse  = "shell_literal"

            [[recipe]]
            name = "shell-pipe"
            [[recipe.install]]
            type = "shell"
            script = "{{ script }}"
            "#,
        )
        .unwrap();
        // Even a genuine pipeline is refused when the switch is off.
        assert!(matches!(
            desugar("curl -fsSL x | bash", &cfg),
            Err(EngineError::NoDesugar(_))
        ));
    }

    #[test]
    fn dispatch_prefix_match() {
        let d = desugar("npm:typescript", &cfg()).unwrap();
        assert_eq!(d.recipe, "npm-global");
        assert_eq!(d.params.get("packages").unwrap().as_str(), Some("typescript"));
    }

    #[test]
    fn dispatch_falls_back_to_shell_literal() {
        let d = desugar("curl -fsSL https://x.sh | bash", &cfg()).unwrap();
        assert_eq!(d.recipe, "shell-pipe");
        assert!(d.params.get("script").unwrap().as_str().unwrap().contains("curl"));
    }

    #[test]
    fn unknown_non_shell_spec_errors_instead_of_running_as_shell() {
        // No `cargo:` prefix in cfg() and not shell-like → hard error, NOT
        // silently executed via the shell_literal catch-all (gap #4).
        assert!(matches!(
            desugar("cargo:ripgrep", &cfg()),
            Err(EngineError::NoDesugar(_))
        ));
        // A genuine pipeline still desugars.
        assert!(desugar("wget -qO- x | sh", &cfg()).is_ok());
    }
}
