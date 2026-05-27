//! Reference `EntrySource` over a host-shaped catalog.json. The host
//! adapter (only host-coupled glue). Ships so the engine is usable
//! out of the box and the strangler integration has a drop-in.
//!
//! An entry installs via EITHER a terse `install` spec (desugared) OR an
//! inline `steps` array (the fully-generic path — any pipeline of generic
//! processors, no desugar prefix / no engine code). `steps` are pre-parsed
//! at load so config errors surface immediately, not mid-install.

use crate::orchestrator::{EntryRef, EntrySource};
use crate::step::Step;
use crate::wizard::InputDecl;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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
    /// Availability / install guard. Skipped (not failed) when false.
    #[serde(default)]
    condition: Option<String>,
    /// Declared inputs (keys/tokens) the entry needs; sourced into the
    /// wizard's `selected.inputs` page.
    #[serde(default)]
    requires_input: Vec<InputDecl>,
    /// Sugar: auto-append a `check_command` verify step for this binary.
    #[serde(default)]
    provides_command: Option<String>,
    // ── optional wizard metadata (engine ignores these) ───────────────────
    #[serde(default, alias = "category")]
    group: Option<String>,
    /// Display label passthrough (engine ignores; wizard uses as label).
    #[serde(default)]
    name: Option<String>,
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
    pub condition: Option<String>,
    pub requires_input: Vec<InputDecl>,
    pub group: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub default: bool,
}

/// A selectable catalog option for the wizard (kind = cli/tools/plugins).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogOption {
    pub key: String,
    pub kind: &'static str,
    pub group: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub default: bool,
    pub condition: Option<String>,
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
    let mut steps = if raw.steps.is_empty() {
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
    if let Some(cmd) = &raw.provides_command {
        let mut m = serde_json::Map::new();
        m.insert("type".into(), Value::String("check_command".into()));
        m.insert("program".into(), Value::String(cmd.clone()));
        m.insert(
            "on_missing".into(),
            Value::String(format!("WARNING: '{cmd}' not found after install")),
        );
        let verify = Step::from_json(m)?;
        match &mut steps {
            Some(s) => s.push(verify),
            None => steps = Some(vec![verify]),
        }
    }
    Ok(CatalogEntry {
        key: raw.key,
        kind,
        spec: raw.install,
        steps,
        deps: raw.dependencies,
        post_install: raw.post_install,
        condition: raw.condition,
        requires_input: raw.requires_input,
        group: raw.group,
        name: raw.name,
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
                name: e.name.clone(),
                description: e.description.clone(),
                default: e.default,
                condition: e.condition.clone(),
            })
            .collect();
        v.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| a.key.cmp(&b.key))
        });
        v
    }

    /// Like `options`, but if `group_order` is non-empty, groups are ordered
    /// by that list first (unlisted groups after, alphabetical), then `key`
    /// within a group. Empty `group_order` ⇒ identical to `options`.
    pub fn options_ordered(&self, kind: &str, group_order: &[String]) -> Vec<CatalogOption> {
        let mut v = self.options(kind);
        if group_order.is_empty() {
            return v;
        }
        let rank = |g: &Option<String>| -> usize {
            g.as_deref()
                .and_then(|name| group_order.iter().position(|x| x == name))
                .unwrap_or(group_order.len())
        };
        v.sort_by(|a, b| {
            let (ra, rb) = (rank(&a.group), rank(&b.group));
            ra.cmp(&rb).then_with(|| {
                if ra == group_order.len() {
                    // unlisted: group name alphabetical, then key
                    let ag = a.group.as_deref().unwrap_or("");
                    let bg = b.group.as_deref().unwrap_or("");
                    ag.cmp(bg).then_with(|| a.key.cmp(&b.key))
                } else {
                    a.key.cmp(&b.key)
                }
            })
        });
        v
    }

    /// Union of `requires_input` over the selected entries, deduped by `id`
    /// (first declaration wins), preserving selection order.
    pub fn required_inputs(&self, selected_keys: &[String]) -> Vec<InputDecl> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<InputDecl> = Vec::new();
        for key in selected_keys {
            let Some(e) = self.entries.get(key) else {
                continue;
            };
            for decl in &e.requires_input {
                if seen.insert(decl.id.clone()) {
                    out.push(decl.clone());
                }
            }
        }
        out
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
            condition: e.condition.clone(),
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

    #[test]
    fn category_alias_maps_to_group() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[{"key":"t","category":"runtime","install":"npm:x"}]}"#,
        )
        .unwrap();
        let opt = &cat.options("tools")[0];
        assert_eq!(opt.group.as_deref(), Some("runtime"));
    }

    #[test]
    fn name_field_accepted_as_label() {
        let cat = Catalog::from_json_str(
            r#"{ "clis":[{"key":"claude","name":"Claude CLI","install":"npm:x"}]}"#,
        )
        .unwrap();
        assert_eq!(cat.options("cli")[0].name.as_deref(), Some("Claude CLI"));
    }

    #[test]
    fn unknown_fields_do_not_cause_parse_error() {
        let cat = Catalog::from_json_str(
            r#"{ "clis":[{"key":"claude","install":"npm:x",
                 "oauth_supported":true,"bin":"claude","supported_clis":["claude"]}]}"#,
        )
        .unwrap();
        assert!(cat.entry("claude").is_some());
    }

    #[test]
    fn condition_field_round_trips() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[{"key":"t","install":"npm:x","condition":"${OS} == 'linux'"}]}"#,
        )
        .unwrap();
        assert_eq!(
            cat.entry("t").unwrap().condition.as_deref(),
            Some("${OS} == 'linux'")
        );
        assert_eq!(
            cat.options("tools")[0].condition.as_deref(),
            Some("${OS} == 'linux'")
        );
    }

    #[test]
    fn requires_input_parses() {
        let cat = Catalog::from_json_str(
            r#"{ "clis":[{"key":"c","install":"npm:x",
                 "requires_input":[{"id":"TOK","type":"secret","required":true}]}]}"#,
        )
        .unwrap();
        let inputs = cat.required_inputs(&["c".into()]);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].id, "TOK");
    }

    #[test]
    fn required_inputs_union_dedup_first_wins() {
        let cat = Catalog::from_json_str(
            r#"{ "clis":[
                 {"key":"a","install":"npm:x","requires_input":[
                    {"id":"SHARED","type":"secret","prompt":"from-a"},
                    {"id":"A_ONLY","type":"text"}]},
                 {"key":"b","install":"npm:y","requires_input":[
                    {"id":"SHARED","type":"secret","prompt":"from-b"},
                    {"id":"B_ONLY","type":"text"}]}]}"#,
        )
        .unwrap();
        let inputs = cat.required_inputs(&["a".into(), "b".into()]);
        let ids: Vec<&str> = inputs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["SHARED", "A_ONLY", "B_ONLY"]);
        assert_eq!(inputs[0].prompt.as_deref(), Some("from-a"));
    }

    #[test]
    fn required_inputs_empty_selection() {
        let cat = Catalog::from_json_str(
            r#"{ "clis":[{"key":"a","install":"npm:x","requires_input":[{"id":"X","type":"text"}]}]}"#,
        )
        .unwrap();
        assert!(cat.required_inputs(&[]).is_empty());
    }

    #[test]
    fn options_ordered_respects_explicit_order() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[
                 {"key":"z","group":"second","install":"npm:z"},
                 {"key":"a","group":"first","install":"npm:a"}]}"#,
        )
        .unwrap();
        let order = vec!["first".to_string(), "second".to_string()];
        let v = cat.options_ordered("tools", &order);
        assert_eq!(v[0].key, "a");
        assert_eq!(v[1].key, "z");
    }

    #[test]
    fn options_ordered_unlisted_after_listed_alpha() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[
                 {"key":"k1","group":"zzz","install":"npm:1"},
                 {"key":"k2","group":"aaa","install":"npm:2"},
                 {"key":"k3","group":"listed","install":"npm:3"}]}"#,
        )
        .unwrap();
        let order = vec!["listed".to_string()];
        let v = cat.options_ordered("tools", &order);
        assert_eq!(v[0].group.as_deref(), Some("listed"));
        assert_eq!(v[1].group.as_deref(), Some("aaa"));
        assert_eq!(v[2].group.as_deref(), Some("zzz"));
    }

    #[test]
    fn options_ordered_empty_matches_options() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[
                 {"key":"b","group":"g","install":"npm:b"},
                 {"key":"a","group":"g","install":"npm:a"}]}"#,
        )
        .unwrap();
        assert_eq!(
            cat.options_ordered("tools", &[]),
            cat.options("tools")
        );
    }

    #[test]
    fn provides_command_appends_check_command_step() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[{"key":"t","steps":[{"type":"shell","script":"true"}],
                 "provides_command":"rg"}]}"#,
        )
        .unwrap();
        let steps = cat.entry("t").unwrap().steps.unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].kind, "check_command");
        assert_eq!(steps[1].param_str("program"), Some("rg"));
    }

    #[test]
    fn provides_command_alone_creates_verify_step() {
        let cat = Catalog::from_json_str(
            r#"{ "tools":[{"key":"t","install":"npm:x","provides_command":"foo"}]}"#,
        )
        .unwrap();
        let steps = cat.entry("t").unwrap().steps.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, "check_command");
    }
}
