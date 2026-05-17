//! Reference `EntrySource` over a codetainyrrr-shaped catalog.json. The host
//! adapter (only codetainyrrr-coupled glue). Ships so the engine is usable
//! out of the box and the strangler integration has a drop-in.
//!
//! An entry installs via EITHER a terse `install` spec (desugared) OR an
//! inline `steps` array (the fully-generic path — any pipeline of generic
//! processors, no desugar prefix / no engine code). `steps` are pre-parsed
//! at load so config errors surface immediately, not mid-install.

use crate::orchestrator::{EntryRef, EntrySource};
use crate::step::Step;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Default, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    clis: Vec<RawEntry>,
    #[serde(default)]
    tools: Vec<RawEntry>,
    #[serde(default)]
    plugins: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    key: String,
    /// Terse spec. `None` ⇒ inline `steps` or a meta entry.
    #[serde(default)]
    install: Option<String>,
    /// Inline generic pipeline (mutually exclusive with `install`).
    #[serde(default)]
    steps: Vec<Value>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    post_install: Vec<String>,
    // ── optional wizard metadata (engine ignores these) ───────────────────
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default: bool,
}

#[derive(Debug)]
pub struct CatalogEntry {
    pub key: String,
    pub kind: &'static str,
    pub spec: Option<String>,
    pub steps: Option<Vec<Step>>,
    pub deps: Vec<String>,
    pub post_install: Vec<String>,
    pub group: Option<String>,
    pub description: Option<String>,
    pub default: bool,
}

/// A selectable catalog option for the wizard (kind = cli/tools/plugins).
#[derive(Debug, Clone)]
pub struct CatalogOption {
    pub key: String,
    pub kind: &'static str,
    pub group: Option<String>,
    pub description: Option<String>,
    pub default: bool,
}

#[derive(Debug, Default)]
pub struct Catalog {
    /// Keyed for O(1) lookup; a key duplicated across any list is a
    /// load-time error (silent first-match was a real data-integrity bug).
    entries: HashMap<String, CatalogEntry>,
}

fn resolve(raw: RawEntry, kind: &'static str) -> crate::Result<CatalogEntry> {
    if raw.install.is_some() && !raw.steps.is_empty() {
        return Err(crate::EngineError::Config(format!(
            "catalog entry '{}' sets both `install` and `steps`",
            raw.key
        )));
    }
    let steps = if raw.steps.is_empty() {
        None
    } else {
        let mut out = Vec::with_capacity(raw.steps.len());
        for v in raw.steps {
            match v {
                Value::Object(m) => out.push(Step::from_json(m)?),
                _ => {
                    return Err(crate::EngineError::Config(format!(
                        "catalog entry '{}': each step must be an object",
                        raw.key
                    )))
                }
            }
        }
        Some(out)
    };
    Ok(CatalogEntry {
        key: raw.key,
        kind,
        spec: raw.install,
        steps,
        deps: raw.dependencies,
        post_install: raw.post_install,
        group: raw.group,
        description: raw.description,
        default: raw.default,
    })
}

impl Catalog {
    pub fn from_json_str(s: &str) -> crate::Result<Self> {
        let raw: RawCatalog =
            serde_json::from_str(s).map_err(|e| crate::EngineError::Config(e.to_string()))?;
        let mut entries: HashMap<String, CatalogEntry> = HashMap::new();
        let lists = [
            (raw.clis, "cli"),
            (raw.tools, "tools"),
            (raw.plugins, "plugins"),
        ];
        for (list, kind) in lists {
            for raw_entry in list {
                let e = resolve(raw_entry, kind)?;
                if let Some(prev) = entries.insert(e.key.clone(), e) {
                    return Err(crate::EngineError::Config(format!(
                        "catalog has duplicate key '{}' (across clis/tools/plugins)",
                        prev.key
                    )));
                }
            }
        }
        Ok(Self { entries })
    }

    /// Selectable options of a given kind ("cli"/"tools"/"plugins"), sorted
    /// by group then key — the wizard's option source.
    pub fn options(&self, kind: &str) -> Vec<CatalogOption> {
        let mut v: Vec<CatalogOption> = self
            .entries
            .values()
            .filter(|e| e.kind == kind)
            .map(|e| CatalogOption {
                key: e.key.clone(),
                kind: e.kind,
                group: e.group.clone(),
                description: e.description.clone(),
                default: e.default,
            })
            .collect();
        v.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| a.key.cmp(&b.key))
        });
        v
    }
}

impl EntrySource for Catalog {
    fn entry(&self, key: &str) -> Option<EntryRef> {
        let e = self.entries.get(key)?;
        Some(EntryRef {
            kind: e.kind.into(),
            spec: e.spec.clone(),
            steps: e.steps.clone(),
            deps: e.deps.clone(),
            post_install: e.post_install.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_list_to_its_sentinel_kind() {
        let cat = Catalog::from_json_str(
            r#"{
              "clis":   [{"key":"claude","install":"npm:x"}],
              "tools":  [{"key":"node","install":"nvm:lts","dependencies":[]},
                         {"key":"meta","dependencies":["node"]}],
              "plugins":[{"key":"p","install":"marketplace:o/r:p","post_install":["echo hi"]}]
            }"#,
        )
        .unwrap();
        assert_eq!(cat.entry("claude").unwrap().kind, "cli");
        assert_eq!(cat.entry("claude").unwrap().spec.as_deref(), Some("npm:x"));
        let meta = cat.entry("meta").unwrap();
        assert_eq!(meta.kind, "tools");
        assert!(meta.spec.is_none() && meta.steps.is_none());
        assert_eq!(meta.deps, vec!["node"]);
        assert_eq!(
            cat.entry("p").unwrap().post_install,
            vec!["echo hi".to_string()]
        );
        assert!(cat.entry("absent").is_none());
    }

    #[test]
    fn inline_steps_are_parsed_and_exposed() {
        let cat = Catalog::from_json_str(
            r#"{ "tools": [{
                "key": "mytool",
                "dependencies": ["node"],
                "steps": [
                    {"type":"shell","script":"git clone x y","dir":"~/w"},
                    {"type":"merge_json","target":"~/.claude.json","command":"echo {}"}
                ]
            }]}"#,
        )
        .unwrap();
        let e = cat.entry("mytool").unwrap();
        assert!(e.spec.is_none());
        let steps = e.steps.unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, "shell");
        assert_eq!(steps[0].param_str("dir"), Some("~/w"));
        assert_eq!(steps[1].kind, "merge_json");
        assert_eq!(e.deps, vec!["node"]);
    }

    #[test]
    fn install_and_steps_are_mutually_exclusive() {
        let err = Catalog::from_json_str(
            r#"{ "tools":[{"key":"bad","install":"npm:x","steps":[{"type":"shell","script":"true"}]}]}"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("both"));
    }

    #[test]
    fn duplicate_key_across_lists_is_a_load_error() {
        let err = Catalog::from_json_str(
            r#"{ "clis":[{"key":"dup","install":"npm:x"}],
                 "tools":[{"key":"dup","install":"nvm:lts"}] }"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("duplicate key 'dup'"));
    }

    #[test]
    fn malformed_inline_step_fails_at_load() {
        let err = Catalog::from_json_str(
            r#"{ "tools":[{"key":"bad","steps":[{"script":"missing type"}]}]}"#,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("type"));
    }
}
