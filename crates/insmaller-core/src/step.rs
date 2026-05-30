use crate::error::{EngineError, Result};
use serde_json::{Map, Value};

/// Recursively convert a TOML value to JSON. The config format (TOML) must
/// not leak past parse: `Step.params` is JSON so every processor + the plugin
/// transport speak one type (and it matches the JSON `EntrySource` boundary).
pub fn toml_to_json(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => Value::from(f),
        toml::Value::Boolean(b) => Value::from(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.into_iter().map(|(k, x)| (k, toml_to_json(x))).collect())
        }
    }
}

/// One declarative action in a package pipeline. `kind` selects the
/// processor; `params` are processor-specific and rendered against `Ctx`
/// before the processor runs. Built from a TOML table (known keys lifted,
/// the rest converted to JSON params — TOML never escapes this type).
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub kind: String,
    pub when: Option<String>,
    /// Inverse of `when`: skip the step when this predicate is truthy.
    pub unless: Option<String>,
    /// Var names that must be present (base ctx or registered) or the step is
    /// skipped — not errored. Lets a step depend on an optional `prompt`
    /// without hitting strict-undefined when the input was not provided.
    pub requires: Vec<String>,
    /// Bind the processor's `value` output under this name for later steps.
    pub register_as: Option<String>,
    /// Value gate: after the step runs and produces a scalar value, abort the
    /// pipeline unless that value equals this (rendered through `Ctx`, so
    /// `confirm = "{{ project_name }}"` works). Empty/absent = no gate. A step
    /// that produces no scalar value — a skipped optional input, or any
    /// processor that returns no value (`shell`/`exec`/`copy`/… all do) — is a
    /// no-op, NOT an abort. So `confirm` is only meaningful on value-producing
    /// steps: `prompt`, `input`, and `save_input`. The orchestrator enforces
    /// it (not the processor) so every such step gets it uniformly.
    pub confirm: Option<String>,
    pub continue_on_error: bool,
    /// Per-step wall-clock timeout in seconds (engine-applied, all processors).
    pub timeout: Option<u64>,
    /// Re-run the step up to this many extra times on failure.
    pub retries: u32,
    pub params: Map<String, Value>,
}

impl Step {
    /// Recipe steps (TOML). Converts to JSON once, then `from_json` — the
    /// single source of step parsing (also reused for catalog inline steps).
    pub fn from_table(t: toml::Table) -> Result<Step> {
        match toml_to_json(toml::Value::Table(t)) {
            Value::Object(m) => Self::from_json(m),
            _ => unreachable!("a Table converts to a JSON object"),
        }
    }

    /// Catalog inline steps (JSON). Lifts the known keys, the rest become
    /// `params`. One canonical parser for both config formats.
    pub fn from_json(mut m: Map<String, Value>) -> Result<Step> {
        fn take_str(m: &mut Map<String, Value>, k: &str) -> Result<Option<String>> {
            match m.remove(k) {
                Some(Value::String(s)) => Ok(Some(s)),
                None => Ok(None),
                Some(_) => Err(EngineError::Config(format!("step `{k}` must be a string"))),
            }
        }
        let kind = take_str(&mut m, "type")?
            .ok_or_else(|| EngineError::Config("step missing `type`".into()))?;
        let when = take_str(&mut m, "when")?;
        let unless = take_str(&mut m, "unless")?;
        let register_as = take_str(&mut m, "register_as")?;
        // Empty string ⇒ no gate (a literal empty expected value is
        // unreachable anyway: value-producing steps emit non-empty values).
        let confirm = take_str(&mut m, "confirm")?.filter(|s| !s.is_empty());
        let requires = match m.remove("requires") {
            Some(Value::Array(a)) => a
                .into_iter()
                .map(|v| {
                    v.as_str().map(String::from).ok_or_else(|| {
                        EngineError::Config("step `requires` entries must be strings".into())
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            Some(Value::String(s)) => vec![s],
            None => vec![],
            Some(_) => {
                return Err(EngineError::Config(
                    "step `requires` must be a string or array of strings".into(),
                ))
            }
        };
        let timeout = match m.remove("timeout") {
            Some(Value::Number(n)) => match n.as_u64() {
                Some(v) if v > 0 => Some(v),
                _ => None,
            },
            None => None,
            Some(_) => {
                return Err(EngineError::Config(
                    "step `timeout` must be a positive integer (seconds)".into(),
                ))
            }
        };
        let retries = match m.remove("retries") {
            // Strict like `timeout`: reject negative/non-integer rather than
            // silently clamping to 0; saturate huge values at u32::MAX.
            Some(Value::Number(n)) => match n.as_u64() {
                Some(v) => v.min(u32::MAX as u64) as u32,
                None => {
                    return Err(EngineError::Config(
                        "step `retries` must be a non-negative integer".into(),
                    ))
                }
            },
            None => 0,
            Some(_) => {
                return Err(EngineError::Config(
                    "step `retries` must be a non-negative integer".into(),
                ))
            }
        };
        let continue_on_error = match m.remove("continue_on_error") {
            Some(Value::Bool(b)) => b,
            None => false,
            Some(_) => {
                return Err(EngineError::Config(
                    "step `continue_on_error` must be a bool".into(),
                ))
            }
        };
        Ok(Step {
            kind,
            when,
            unless,
            requires,
            register_as,
            confirm,
            continue_on_error,
            timeout,
            retries,
            params: m,
        })
    }

    pub fn param_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(|v| v.as_str())
    }
    pub fn param_bool(&self, key: &str) -> Option<bool> {
        self.params.get(key).and_then(|v| v.as_bool())
    }
    pub fn param_i64(&self, key: &str) -> Option<i64> {
        self.params.get(key).and_then(|v| v.as_i64())
    }
    pub fn param_array(&self, key: &str) -> Option<&Vec<Value>> {
        self.params.get(key).and_then(|v| v.as_array())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(src: &str) -> toml::Table {
        src.parse().unwrap()
    }

    #[test]
    fn lifts_known_keys_rest_is_params() {
        let s = Step::from_table(table(
            r#"
            type = "exec"
            when = "{{ os }} == 'linux'"
            continue_on_error = true
            program = "npm"
            args = ["install", "-g", "foo"]
            "#,
        ))
        .unwrap();
        assert_eq!(s.kind, "exec");
        assert_eq!(s.when.as_deref(), Some("{{ os }} == 'linux'"));
        assert!(s.continue_on_error);
        assert_eq!(s.param_str("program"), Some("npm"));
        assert!(s.params.get("type").is_none());
        assert!(s.params.get("args").unwrap().is_array());
    }

    #[test]
    fn defaults_when_optional_keys_absent() {
        let s = Step::from_table(table(
            r#"
            type = "shell"
            script = "echo hi"
            "#,
        ))
        .unwrap();
        assert_eq!(s.kind, "shell");
        assert_eq!(s.when, None);
        assert!(!s.continue_on_error);
    }

    #[test]
    fn missing_type_is_config_error() {
        let err = Step::from_table(table(r#"script = "echo hi""#)).unwrap_err();
        assert!(matches!(err, EngineError::Config(_)));
    }

    #[test]
    fn confirm_parses_and_empty_is_no_gate() {
        let s = Step::from_table(table("type=\"prompt\"\nconfirm=\"RESET\"")).unwrap();
        assert_eq!(s.confirm.as_deref(), Some("RESET"));
        // Empty string ⇒ no gate (filtered to None), and it's lifted out of
        // params so a processor can't re-read it.
        let e = Step::from_table(table("type=\"prompt\"\nconfirm=\"\"")).unwrap();
        assert_eq!(e.confirm, None);
        assert!(e.params.get("confirm").is_none());
        // Absent ⇒ None.
        let n = Step::from_table(table("type=\"prompt\"")).unwrap();
        assert_eq!(n.confirm, None);
    }

    #[test]
    fn retries_negative_errors_huge_saturates() {
        // negative → hard error (was a silent clamp to 0)
        assert!(Step::from_table(table("type=\"shell\"\nscript=\"x\"\nretries=-1")).is_err());
        // overflow → saturates to u32::MAX, no panic
        let s = Step::from_table(table(
            "type=\"shell\"\nscript=\"x\"\nretries=99999999999",
        ))
        .unwrap();
        assert_eq!(s.retries, u32::MAX);
        // normal value preserved
        let s2 = Step::from_table(table("type=\"shell\"\nscript=\"x\"\nretries=3")).unwrap();
        assert_eq!(s2.retries, 3);
    }
}
