//! Optional pages/wizard layer. Pure schema + condition eval + resolver; the
//! interactive rendering is injected via `Answerer` (mirrors the
//! `InputResolver` keystone — `StaticAnswerer` is non-blocking for
//! unattended/tests, the CLI supplies a stdin one). Output is the set of
//! catalog keys to install + a vars map (seeded into the env so the engine's
//! `prompt`/`save_input`/`EnvResolver` pick them up).
//!
//! Condition syntax mirrors codetainyrrr wizard.json:
//!   `${VAR} == 'lit'` · `${VAR} != 'lit'` · `${VAR} in 'a,b,c'` ·
//!   `'item' in ${VAR}` (CSV membership; a multiselect joins with ',').

use crate::error::{EngineError, Result};
use crate::json_catalog::Catalog;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Multiselect,
    SingleSelect,
    Text,
    Secret,
    Path,
    Toggle,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub id: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
    /// `catalog.tools` | `catalog.clis` | `catalog.plugins` — options come
    /// from the catalog (and the answers ARE keys to install).
    #[serde(default)]
    pub source: Option<String>,
    /// Static options (alternative to `source`).
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub condition: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Field `source` value: expand into one synthetic field per declared input
/// of the currently-selected entries (P1-A).
pub const SELECTED_INPUTS: &str = "selected.inputs";

/// An input (key/token/cred) an entry declares it requires. Sourced into a
/// wizard page via `source = "selected.inputs"` — one synthetic [`Field`] per
/// declared input of the currently-selected entries.
#[derive(Debug, Clone, Deserialize)]
pub struct InputDecl {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: FieldType,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
}

impl InputDecl {
    /// Synthetic field for this declaration. `condition` is applied by the
    /// caller before expansion, so the produced field carries none.
    pub fn to_field(&self) -> Field {
        Field {
            id: self.id.clone(),
            field_type: self.r#type,
            prompt: self.prompt.clone(),
            default: self.default.clone(),
            required: self.required,
            source: None,
            options: Vec::new(),
            condition: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default, rename = "field")]
    pub fields: Vec<Field>,
}

#[derive(Debug, Deserialize)]
pub struct WizardDef {
    #[serde(default, rename = "page")]
    pub pages: Vec<Page>,
}

impl WizardDef {
    #[allow(clippy::should_implement_trait)] // inherent ctor: EngineError, not a FromStr::Err
    pub fn from_str(toml_src: &str) -> Result<Self> {
        toml::from_str(toml_src).map_err(|e| EngineError::Config(format!("wizard: {e}")))
    }
}

/// One selectable option presented for a field.
#[derive(Debug, Clone)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub default: bool,
}

/// An answer for a field. `Skip` = optional field the user declined.
#[derive(Debug, Clone, PartialEq)]
pub enum WizValue {
    Multi(Vec<String>),
    One(String),
    Text(String),
    Bool(bool),
    Skip,
}

/// Injected answer source. `ask` must NEVER block in an unattended context
/// (same contract as `InputResolver`): resolve from config/env or fail fast.
pub trait Answerer {
    fn ask(&self, field: &Field, choices: &[Choice]) -> Result<WizValue>;
}

/// Non-blocking answerer backed by a prepared map (answers file / test).
/// Missing + required → hard error; missing + optional → default or Skip.
pub struct StaticAnswerer(pub Map<String, Value>);

impl Answerer for StaticAnswerer {
    fn ask(&self, field: &Field, _choices: &[Choice]) -> Result<WizValue> {
        match self.0.get(&field.id) {
            Some(Value::Array(a)) => Ok(WizValue::Multi(
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            )),
            Some(Value::Bool(b)) => Ok(WizValue::Bool(*b)),
            Some(Value::String(s)) => Ok(match field.field_type {
                FieldType::SingleSelect => WizValue::One(s.clone()),
                _ => WizValue::Text(s.clone()),
            }),
            Some(_) | None => {
                if let Some(d) = &field.default {
                    return Ok(WizValue::Text(d.clone()));
                }
                if field.required {
                    Err(EngineError::MissingInput(field.id.clone()))
                } else {
                    Ok(WizValue::Skip)
                }
            }
        }
    }
}

/// Result of running the wizard.
#[derive(Debug, Default, PartialEq)]
pub struct WizardOutcome {
    /// Catalog keys to install (from catalog-sourced select fields).
    pub selected_keys: Vec<String>,
    /// All field answers (string scalars seeded into env by the host).
    pub vars: Map<String, Value>,
}

fn var_as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(","),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// Dotted numeric version (`v` prefix and trailing non-digits per component
/// tolerated). `None` ⇒ not version-like, caller falls back to string compare.
fn parse_ver(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    if s.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for p in s.split('.') {
        let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        out.push(digits.parse::<u64>().ok()?);
    }
    Some(out)
}

/// Semver-ish ordering. `None` ⇒ either side isn't version-like.
fn version_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (av, bv) = (parse_ver(a)?, parse_ver(b)?);
    let n = av.len().max(bv.len());
    for i in 0..n {
        let (x, y) = (
            av.get(i).copied().unwrap_or(0),
            bv.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return Some(x.cmp(&y));
        }
    }
    Some(std::cmp::Ordering::Equal)
}

/// Evaluate a condition against collected vars. Mirrors codetainyrrr, plus
/// version-compare operators (`>= <= > <`, and semver-aware `== !=`) — the
/// single expression grammar reused by entries, item conditions, and pages.
pub fn eval_condition(expr: &str, vars: &Map<String, Value>) -> bool {
    use std::cmp::Ordering;
    let s = expr.trim();
    let get = |name: &str| -> String {
        vars.get(name.trim()).map(var_as_str).unwrap_or_default()
    };
    let unwrap_var = |t: &str| {
        t.trim()
            .trim_start_matches("${")
            .trim_end_matches('}')
            .trim()
            .to_string()
    };
    let lit = |t: &str| t.trim().trim_matches('\'').trim_matches('"').to_string();

    if let Some((l, r)) = s.split_once(">=") {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return matches!(
            version_cmp(&lv, &rv),
            Some(Ordering::Greater | Ordering::Equal)
        );
    }
    if let Some((l, r)) = s.split_once("<=") {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return matches!(
            version_cmp(&lv, &rv),
            Some(Ordering::Less | Ordering::Equal)
        );
    }
    if let Some((l, r)) = s.split_once("==") {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return match version_cmp(&lv, &rv) {
            Some(o) => o == Ordering::Equal,
            None => lv == rv,
        };
    }
    if let Some((l, r)) = s.split_once("!=") {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return match version_cmp(&lv, &rv) {
            Some(o) => o != Ordering::Equal,
            None => lv != rv,
        };
    }
    if let Some((l, r)) = s.split_once('>') {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return matches!(version_cmp(&lv, &rv), Some(Ordering::Greater));
    }
    if let Some((l, r)) = s.split_once('<') {
        let (lv, rv) = (get(&unwrap_var(l)), lit(r));
        return matches!(version_cmp(&lv, &rv), Some(Ordering::Less));
    }
    if let Some((l, r)) = s.split_once(" in ") {
        let lt = l.trim();
        if lt.starts_with('\'') || lt.starts_with('"') {
            // 'item' in ${VAR}  → CSV membership in VAR
            let item = lit(lt);
            let hay = get(&unwrap_var(r));
            return hay.split(',').any(|x| x.trim() == item);
        }
        // ${VAR} in 'a,b,c'
        let val = get(&unwrap_var(lt));
        return lit(r).split(',').any(|x| x.trim() == val);
    }
    // Bare `${VAR}` / name → truthy if non-empty and not false/0.
    let v = get(&unwrap_var(s));
    !(v.is_empty() || v == "false" || v == "0")
}

#[cfg(test)]
fn choices_for(field: &Field, catalog: &Catalog) -> Vec<Choice> {
    choices_for_vars(field, catalog, &Map::new(), &[])
}

/// Choices for a field, dropping catalog options whose entry `condition`
/// evaluates false against `vars`, and ordering groups by `group_order`
/// (empty ⇒ default group/key sort).
pub fn choices_for_vars(
    field: &Field,
    catalog: &Catalog,
    vars: &Map<String, Value>,
    group_order: &[String],
) -> Vec<Choice> {
    if let Some(src) = &field.source {
        if let Some(kind) = src.strip_prefix("catalog.") {
            // catalog.tools → kind "tools"; catalog.clis → "cli".
            let kind = if kind == "clis" { "cli" } else { kind };
            return catalog
                .options_ordered(kind, group_order)
                .into_iter()
                .filter(|o| {
                    o.condition
                        .as_deref()
                        .map(|c| eval_condition(c, vars))
                        .unwrap_or(true)
                })
                .map(|o| Choice {
                    label: match (&o.group, &o.description) {
                        (Some(g), Some(d)) => format!("[{g}] {} — {d}", o.key),
                        (Some(g), None) => format!("[{g}] {}", o.key),
                        (None, Some(d)) => format!("{} — {d}", o.key),
                        (None, None) => o.key.clone(),
                    },
                    value: o.key,
                    default: o.default,
                })
                .collect();
        }
    }
    field
        .options
        .iter()
        .map(|v| Choice {
            value: v.clone(),
            label: v.clone(),
            default: false,
        })
        .collect()
}

fn is_catalog_source(field: &Field) -> bool {
    field
        .source
        .as_deref()
        .map(|s| s.starts_with("catalog."))
        .unwrap_or(false)
}

/// Run the wizard: walk pages/fields honoring conditions, collect answers,
/// emit the catalog keys to install + the vars map.
pub fn run_wizard(
    def: &WizardDef,
    catalog: &Catalog,
    answerer: &dyn Answerer,
    group_order: &[String],
) -> Result<WizardOutcome> {
    let mut out = WizardOutcome::default();
    for page in &def.pages {
        if let Some(c) = &page.condition {
            if !eval_condition(c, &out.vars) {
                continue;
            }
        }
        for field in &page.fields {
            if let Some(c) = &field.condition {
                if !eval_condition(c, &out.vars) {
                    continue;
                }
            }
            // `selected.inputs` expands in place into one synthetic field per
            // declared input of the currently-selected entries.
            if field.source.as_deref() == Some(SELECTED_INPUTS) {
                for decl in catalog.required_inputs(&out.selected_keys) {
                    if decl
                        .condition
                        .as_deref()
                        .is_some_and(|c| !eval_condition(c, &out.vars))
                    {
                        continue;
                    }
                    let synthetic = decl.to_field();
                    let stored = match answerer.ask(&synthetic, &[])? {
                        WizValue::Skip => continue,
                        WizValue::Multi(v) => {
                            Value::Array(v.into_iter().map(Value::String).collect())
                        }
                        WizValue::One(s) | WizValue::Text(s) => Value::String(s),
                        WizValue::Bool(b) => Value::Bool(b),
                    };
                    out.vars.insert(synthetic.id.clone(), stored);
                }
                continue;
            }
            let choices = choices_for_vars(field, catalog, &out.vars, group_order);
            let ans = answerer.ask(field, &choices)?;
            let stored = match ans {
                WizValue::Skip => continue,
                WizValue::Multi(v) => {
                    if is_catalog_source(field) {
                        out.selected_keys.extend(v.iter().cloned());
                    }
                    Value::Array(v.into_iter().map(Value::String).collect())
                }
                WizValue::One(s) => {
                    if is_catalog_source(field) && !s.is_empty() {
                        out.selected_keys.push(s.clone());
                    }
                    Value::String(s)
                }
                WizValue::Text(s) => Value::String(s),
                WizValue::Bool(b) => Value::Bool(b),
            };
            out.vars.insert(field.id.clone(), stored);
        }
    }
    // De-dup selected keys, preserve order.
    let mut seen = std::collections::HashSet::new();
    out.selected_keys.retain(|k| seen.insert(k.clone()));
    Ok(out)
}

/// Derive the outcome from already-collected `vars` (used by the navigable
/// session after free back/forward editing — selected keys are recomputed
/// from the *currently active* catalog-source fields, so flipping an earlier
/// answer that hides a later page correctly drops its keys).
pub fn collect_outcome(def: &WizardDef, vars: &Map<String, Value>) -> WizardOutcome {
    let mut out = WizardOutcome {
        vars: vars.clone(),
        ..Default::default()
    };
    for page in &def.pages {
        if page.condition.as_deref().is_some_and(|c| !eval_condition(c, vars)) {
            continue;
        }
        for field in &page.fields {
            if field.condition.as_deref().is_some_and(|c| !eval_condition(c, vars)) {
                continue;
            }
            if !is_catalog_source(field) {
                continue;
            }
            match vars.get(&field.id) {
                Some(Value::Array(a)) => out
                    .selected_keys
                    .extend(a.iter().filter_map(|v| v.as_str().map(String::from))),
                Some(Value::String(s)) if !s.is_empty() => {
                    out.selected_keys.push(s.clone())
                }
                _ => {}
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.selected_keys.retain(|k| seen.insert(k.clone()));
    out
}

/// Navigable wizard state machine for interactive frontends (TUI). Pure: it
/// holds answers and computes which pages/fields are active given current
/// answers, and supports free back/forward. Conditions are re-evaluated on
/// every move, so going back and changing an answer correctly re-gates later
/// pages. Unattended callers keep using `run_wizard` + `StaticAnswerer`.
pub struct WizardSession<'a> {
    def: &'a WizardDef,
    catalog: &'a Catalog,
    vars: Map<String, Value>,
    group_order: Vec<String>,
    /// Index into `def.pages` of the page currently shown.
    idx: usize,
}

impl<'a> WizardSession<'a> {
    pub fn new(def: &'a WizardDef, catalog: &'a Catalog, group_order: Vec<String>) -> Self {
        let mut s = Self {
            def,
            catalog,
            vars: Map::new(),
            group_order,
            idx: 0,
        };
        s.idx = s.next_active_from(0).unwrap_or(def.pages.len());
        s
    }

    /// Catalog keys selected so far (recomputed from currently-active
    /// catalog-source fields).
    pub fn current_selected_keys(&self) -> Vec<String> {
        collect_outcome(self.def, &self.vars).selected_keys
    }

    fn active(&self, i: usize) -> bool {
        self.def.pages.get(i).is_some_and(|p| {
            p.condition
                .as_deref()
                .map(|c| eval_condition(c, &self.vars))
                .unwrap_or(true)
        })
    }
    fn next_active_from(&self, start: usize) -> Option<usize> {
        (start..self.def.pages.len()).find(|&i| self.active(i))
    }
    fn prev_active_before(&self, before: usize) -> Option<usize> {
        (0..before).rev().find(|&i| self.active(i))
    }

    pub fn is_done(&self) -> bool {
        self.idx >= self.def.pages.len()
    }
    pub fn current(&self) -> Option<&'a Page> {
        self.def.pages.get(self.idx)
    }
    /// Visible fields of the current page (field conditions applied). A
    /// `selected.inputs` field expands in place into one synthetic field per
    /// declared input of the currently-selected entries (each gated by its
    /// own `condition`). Returns owned fields because synthetic ones are not
    /// part of `WizardDef`.
    pub fn fields(&self) -> Vec<Field> {
        let Some(p) = self.current() else {
            return Vec::new();
        };
        let mut out: Vec<Field> = Vec::new();
        for f in &p.fields {
            if f.condition
                .as_deref()
                .is_some_and(|c| !eval_condition(c, &self.vars))
            {
                continue;
            }
            if f.source.as_deref() == Some(SELECTED_INPUTS) {
                let keys = self.current_selected_keys();
                for decl in self.catalog.required_inputs(&keys) {
                    if decl
                        .condition
                        .as_deref()
                        .is_some_and(|c| !eval_condition(c, &self.vars))
                    {
                        continue;
                    }
                    out.push(decl.to_field());
                }
                continue;
            }
            out.push(f.clone());
        }
        out
    }
    pub fn choices(&self, field: &Field) -> Vec<Choice> {
        choices_for_vars(field, self.catalog, &self.vars, &self.group_order)
    }
    /// Prior answer for a field (so back-nav re-renders with it selected).
    pub fn answer_for(&self, id: &str) -> Option<&Value> {
        self.vars.get(id)
    }
    pub fn can_back(&self) -> bool {
        self.prev_active_before(self.idx).is_some()
    }
    /// (1-based step among active pages, total active given current answers).
    pub fn progress(&self) -> (usize, usize) {
        let total = (0..self.def.pages.len()).filter(|&i| self.active(i)).count();
        let step = (0..=self.idx.min(self.def.pages.len().saturating_sub(1)))
            .filter(|&i| self.active(i))
            .count();
        (step.max(1), total.max(1))
    }

    /// Store this page's answers (validating required) and advance to the
    /// next active page (or done). `answers` keys are field ids.
    pub fn submit(&mut self, answers: Map<String, Value>) -> Result<()> {
        for f in self.fields() {
            match answers.get(&f.id) {
                Some(v)
                    if !(matches!(v, Value::String(s) if s.is_empty())
                        || matches!(v, Value::Array(a) if a.is_empty())) =>
                {
                    self.vars.insert(f.id.clone(), v.clone());
                }
                _ => {
                    if let Some(v) = answers.get(&f.id) {
                        self.vars.insert(f.id.clone(), v.clone());
                    }
                    if f.required && f.default.is_none() {
                        // empty/missing + required → caller must re-ask.
                        return Err(EngineError::MissingInput(f.id.clone()));
                    }
                }
            }
        }
        self.idx = self
            .next_active_from(self.idx + 1)
            .unwrap_or(self.def.pages.len());
        Ok(())
    }

    /// Persist this page's answers WITHOUT validating or advancing — used
    /// before `back()` so returning to a page shows prior edits.
    pub fn store(&mut self, answers: Map<String, Value>) {
        for f in self.fields() {
            if let Some(v) = answers.get(&f.id) {
                self.vars.insert(f.id.clone(), v.clone());
            }
        }
    }

    pub fn back(&mut self) -> bool {
        match self.prev_active_before(self.idx) {
            Some(i) => {
                self.idx = i;
                true
            }
            None => false,
        }
    }

    pub fn finish(&self) -> WizardOutcome {
        collect_outcome(self.def, &self.vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalog {
        Catalog::from_json_str(
            r#"{ "tools":[
              {"key":"node","group":"runtime","default":true},
              {"key":"ripgrep","group":"cli","description":"fast grep"}
            ], "clis":[{"key":"claude","install":"npm:x"}] }"#,
        )
        .unwrap()
    }

    const WIZ: &str = r#"
        [[page]]
        id = "tools"
        title = "Dev tools"
        [[page.field]]
        id = "INSTALL_TOOLS"
        type = "multiselect"
        source = "catalog.tools"

        [[page]]
        id = "keys"
        title = "Keys"
        condition = "'ripgrep' in ${INSTALL_TOOLS}"
        [[page.field]]
        id = "OPENAI_API_KEY"
        type = "secret"
        required = false
    "#;

    #[test]
    fn parses_and_lists_catalog_choices() {
        let d = WizardDef::from_str(WIZ).unwrap();
        assert_eq!(d.pages.len(), 2);
        let f = &d.pages[0].fields[0];
        let ch = choices_for(f, &cat());
        // sorted by group then key: cli/ripgrep, runtime/node
        assert_eq!(ch[0].value, "ripgrep");
        assert!(ch[0].label.contains("fast grep"));
        assert_eq!(ch[1].value, "node");
        assert!(ch[1].default);
    }

    #[test]
    fn condition_gates_page_visibility() {
        assert!(eval_condition(
            "'ripgrep' in ${INSTALL_TOOLS}",
            &serde_json::json!({"INSTALL_TOOLS":["ripgrep","node"]})
                .as_object()
                .unwrap()
                .clone()
        ));
        assert!(!eval_condition(
            "${CODING} == 'claude'",
            &serde_json::json!({"CODING":"aider"}).as_object().unwrap().clone()
        ));
        assert!(eval_condition(
            "${CODING} in 'claude,aider'",
            &serde_json::json!({"CODING":"aider"}).as_object().unwrap().clone()
        ));
    }

    #[test]
    fn run_collects_keys_and_skips_gated_page() {
        // pick ripgrep only; the keys page is gated on ripgrep → runs.
        let mut a = Map::new();
        a.insert("INSTALL_TOOLS".into(), serde_json::json!(["ripgrep"]));
        a.insert("OPENAI_API_KEY".into(), Value::String("sk-x".into()));
        let o = run_wizard(
            &WizardDef::from_str(WIZ).unwrap(),
            &cat(),
            &StaticAnswerer(a),
            &[],
        )
        .unwrap();
        assert_eq!(o.selected_keys, vec!["ripgrep"]);
        assert_eq!(o.vars.get("OPENAI_API_KEY").unwrap(), "sk-x");
    }

    #[test]
    fn gated_page_skipped_when_condition_false() {
        let mut a = Map::new();
        a.insert("INSTALL_TOOLS".into(), serde_json::json!(["node"]));
        // OPENAI not provided; its page is gated on ripgrep (not picked) so
        // the required-secret is never asked → no MissingInput error.
        let o = run_wizard(
            &WizardDef::from_str(WIZ).unwrap(),
            &cat(),
            &StaticAnswerer(a),
            &[],
        )
        .unwrap();
        assert_eq!(o.selected_keys, vec!["node"]);
        assert!(o.vars.get("OPENAI_API_KEY").is_none());
    }

    #[test]
    fn required_field_missing_is_fail_fast() {
        // make the keys page always-on by feeding ripgrep, omit the secret
        // but mark it required via a tweaked wizard.
        let wiz = r#"
            [[page]]
            id="k"
            [[page.field]]
            id="TOKEN"
            type="secret"
            required=true
        "#;
        let r = run_wizard(
            &WizardDef::from_str(wiz).unwrap(),
            &cat(),
            &StaticAnswerer(Map::new()),
            &[],
        );
        assert!(matches!(r, Err(EngineError::MissingInput(_))));
    }

    #[test]
    fn session_navigates_forward_and_back_with_recompute() {
        let d = WizardDef::from_str(WIZ).unwrap();
        let c = cat();
        let mut s = WizardSession::new(&d, &c, vec![]);
        // page 1 = tools; progress 1/1 because keys page is gated off (no
        // INSTALL_TOOLS yet → 'ripgrep' in '' is false).
        assert_eq!(s.current().unwrap().id, "tools");
        assert_eq!(s.progress(), (1, 1));
        assert!(!s.can_back());

        // pick ripgrep → keys page becomes active.
        let mut a = Map::new();
        a.insert("INSTALL_TOOLS".into(), serde_json::json!(["ripgrep"]));
        s.submit(a).unwrap();
        assert_eq!(s.current().unwrap().id, "keys");
        assert_eq!(s.progress(), (2, 2));
        assert!(s.can_back());

        // go back, change to node only → keys page must vanish on forward.
        assert!(s.back());
        assert_eq!(s.current().unwrap().id, "tools");
        let mut a2 = Map::new();
        a2.insert("INSTALL_TOOLS".into(), serde_json::json!(["node"]));
        s.submit(a2).unwrap();
        assert!(s.is_done(), "keys page re-gated off after back-edit");
        let o = s.finish();
        assert_eq!(o.selected_keys, vec!["node"]); // ripgrep dropped
    }

    #[test]
    fn session_finish_recomputes_keys_from_active_only() {
        let d = WizardDef::from_str(WIZ).unwrap();
        let c = cat();
        let mut s = WizardSession::new(&d, &c, vec![]);
        let mut a = Map::new();
        a.insert("INSTALL_TOOLS".into(), serde_json::json!(["ripgrep", "node"]));
        s.submit(a).unwrap();
        let mut k = Map::new();
        k.insert("OPENAI_API_KEY".into(), Value::String("sk".into()));
        s.submit(k).unwrap();
        assert!(s.is_done());
        let o = s.finish();
        assert_eq!(o.selected_keys, vec!["ripgrep", "node"]);
        assert_eq!(o.vars.get("OPENAI_API_KEY").unwrap(), "sk");
    }

    fn cat_inputs() -> Catalog {
        Catalog::from_json_str(
            r#"{ "clis":[
              {"key":"alpha","install":"npm:a","requires_input":[
                 {"id":"ALPHA_TOKEN","type":"secret","required":true},
                 {"id":"OAUTH","type":"toggle","required":false,
                  "condition":"${USE_OAUTH} == 'yes'"}]},
              {"key":"beta","install":"npm:b","condition":"${OS} == 'linux'",
               "requires_input":[{"id":"ALPHA_TOKEN","type":"secret"}]}
            ]}"#,
        )
        .unwrap()
    }

    const WIZ_INPUTS: &str = r#"
        [[page]]
        id = "pick"
        [[page.field]]
        id = "clis"
        type = "multiselect"
        source = "catalog.clis"

        [[page]]
        id = "inputs"
        [[page.field]]
        id = "_req"
        type = "text"
        source = "selected.inputs"
    "#;

    #[test]
    fn selected_inputs_expands_synthetic_fields() {
        let mut a = Map::new();
        a.insert("clis".into(), serde_json::json!(["alpha"]));
        a.insert("ALPHA_TOKEN".into(), Value::String("sek".into()));
        let o = run_wizard(
            &WizardDef::from_str(WIZ_INPUTS).unwrap(),
            &cat_inputs(),
            &StaticAnswerer(a),
            &[],
        )
        .unwrap();
        assert_eq!(o.vars.get("ALPHA_TOKEN").unwrap(), "sek");
    }

    #[test]
    fn selected_inputs_condition_gates_field() {
        // OAUTH only asked when USE_OAUTH == yes; here it's not set, so the
        // gated input is skipped and its absence is not an error.
        let mut a = Map::new();
        a.insert("clis".into(), serde_json::json!(["alpha"]));
        a.insert("ALPHA_TOKEN".into(), Value::String("sek".into()));
        let o = run_wizard(
            &WizardDef::from_str(WIZ_INPUTS).unwrap(),
            &cat_inputs(),
            &StaticAnswerer(a),
            &[],
        )
        .unwrap();
        assert!(o.vars.get("OAUTH").is_none());
    }

    #[test]
    fn selected_inputs_required_missing_is_error() {
        let mut a = Map::new();
        a.insert("clis".into(), serde_json::json!(["alpha"]));
        // ALPHA_TOKEN required but not provided.
        let r = run_wizard(
            &WizardDef::from_str(WIZ_INPUTS).unwrap(),
            &cat_inputs(),
            &StaticAnswerer(a),
            &[],
        );
        assert!(matches!(r, Err(EngineError::MissingInput(_))));
    }

    #[test]
    fn selected_inputs_dedup_two_entries_same_id() {
        // alpha+beta both declare ALPHA_TOKEN → asked once.
        let mut a = Map::new();
        a.insert("clis".into(), serde_json::json!(["alpha", "beta"]));
        a.insert("ALPHA_TOKEN".into(), Value::String("one".into()));
        let o = run_wizard(
            &WizardDef::from_str(WIZ_INPUTS).unwrap(),
            &cat_inputs(),
            &StaticAnswerer(a),
            &[],
        )
        .unwrap();
        assert_eq!(o.vars.get("ALPHA_TOKEN").unwrap(), "one");
    }

    #[test]
    fn choices_for_vars_hides_entry_when_condition_false() {
        let c = cat_inputs();
        let f = Field {
            id: "x".into(),
            field_type: FieldType::Multiselect,
            prompt: None,
            default: None,
            required: false,
            source: Some("catalog.clis".into()),
            options: vec![],
            condition: None,
        };
        let mut vars = Map::new();
        vars.insert("OS".into(), Value::String("macos".into()));
        let ch = choices_for_vars(&f, &c, &vars, &[]);
        assert!(ch.iter().all(|x| x.value != "beta"));
        assert!(ch.iter().any(|x| x.value == "alpha"));
    }

    #[test]
    fn choices_for_vars_shows_entry_when_condition_true() {
        let c = cat_inputs();
        let f = Field {
            id: "x".into(),
            field_type: FieldType::Multiselect,
            prompt: None,
            default: None,
            required: false,
            source: Some("catalog.clis".into()),
            options: vec![],
            condition: None,
        };
        let mut vars = Map::new();
        vars.insert("OS".into(), Value::String("linux".into()));
        let ch = choices_for_vars(&f, &c, &vars, &[]);
        assert!(ch.iter().any(|x| x.value == "beta"));
    }

    #[test]
    fn eval_version_ge_le_gt_lt() {
        let v = |k: &str, val: &str| {
            let mut m = Map::new();
            m.insert(k.into(), Value::String(val.into()));
            m
        };
        assert!(eval_condition("${NODE} >= '20'", &v("NODE", "22.3.1")));
        assert!(!eval_condition("${NODE} >= '20'", &v("NODE", "18.9")));
        assert!(eval_condition("${PY} >= '3.10'", &v("PY", "3.12.1")));
        assert!(!eval_condition("${PY} >= '3.10'", &v("PY", "3.9.18")));
        assert!(eval_condition("${V} <= '1.2.3'", &v("V", "1.2.3")));
        assert!(eval_condition("${V} > '1.0'", &v("V", "1.0.1")));
        assert!(!eval_condition("${V} > '1.0'", &v("V", "1.0.0")));
        assert!(eval_condition("${V} < '2'", &v("V", "1.9.9")));
    }

    #[test]
    fn eval_version_eq_semver() {
        let mut m = Map::new();
        m.insert("V".into(), Value::String("1.2.0".into()));
        assert!(eval_condition("${V} == '1.2'", &m)); // 1.2.0 == 1.2
        assert!(!eval_condition("${V} != '1.2'", &m));
    }

    #[test]
    fn eval_version_falls_back_to_string_when_unparseable() {
        let mut m = Map::new();
        m.insert("MODE".into(), Value::String("fast".into()));
        assert!(eval_condition("${MODE} == 'fast'", &m));
        assert!(!eval_condition("${MODE} == 'slow'", &m));
        // version ops on non-version values → false (no panic).
        assert!(!eval_condition("${MODE} >= '1.0'", &m));
    }
}
