//! Optional pages/wizard layer. Pure schema + condition eval + resolver; the
//! interactive rendering is injected via `Answerer` (mirrors the
//! `InputResolver` keystone — `StaticAnswerer` is non-blocking for
//! unattended/tests, the CLI supplies a stdin one). Output is the set of
//! catalog keys to install + a vars map (seeded into the env so the engine's
//! `prompt`/`save_input`/`EnvResolver` pick them up).
//!
//! Condition syntax mirrors the reference installer wizard.json:
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
    /// Collapsed type-to-search selector (differs from `single_select` which stays expanded).
    Dropdown,
    Text,
    /// Multi-line text input.
    Textarea,
    Secret,
    Path,
    Toggle,
    /// ISO 8601 date (`YYYY-MM-DD`).
    Date,
    /// ISO 8601 datetime (`YYYY-MM-DDTHH:MM:SS`).
    Datetime,
}

/// Named value validators for text-like fields (alternative/addition to a raw
/// `pattern` regex).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldFormat {
    Integer,
    Number,
    Alpha,
    Alnum,
    Email,
}

impl FieldFormat {
    fn label(self) -> &'static str {
        match self {
            FieldFormat::Integer => "an integer",
            FieldFormat::Number => "a number",
            FieldFormat::Alpha => "letters only",
            FieldFormat::Alnum => "letters and digits only",
            FieldFormat::Email => "an email address",
        }
    }
    fn accepts(self, v: &str) -> bool {
        match self {
            FieldFormat::Integer => v.parse::<i64>().is_ok(),
            // reject NaN/inf — they parse as f64 but aren't meaningful values
            // and would silently slip past min/max (NaN compares false to both).
            FieldFormat::Number => v.parse::<f64>().map(|n| n.is_finite()).unwrap_or(false),
            FieldFormat::Alpha => v.chars().all(|c| c.is_alphabetic()),
            FieldFormat::Alnum => v.chars().all(|c| c.is_alphanumeric()),
            // intentionally minimal: local@domain.tld shape, no full RFC 5322.
            FieldFormat::Email => {
                let mut parts = v.splitn(2, '@');
                let local = parts.next().unwrap_or("");
                let domain = parts.next().unwrap_or("");
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
            }
        }
    }
}

/// A bound value for `min`/`max` on a `Validate`. Accepts either a bare number
/// (`min = 5`) or a quoted string (`min = "2026-06-01"`), deserialized via
/// serde's untagged enum. Numeric fields use `Num`; date/datetime fields use
/// `Str`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Bound {
    Num(f64),
    Str(String),
}

/// Optional API-call validation block. Lives at the `[[page.field]]` level
/// under the key `api` — e.g. `[page.field.api]`.
///
/// NOTE ON TOML PATH: `Validate` is `#[serde(flatten)]`-ed into `Field`, so
/// its fields (including `api`) are promoted to the `[[page.field]]` level.
/// The spec example shows `[page.field.validate.api]`, but that nesting does
/// NOT match the flattened schema. The correct TOML path is `[page.field.api]`.
#[derive(Debug, Clone, Deserialize)]
pub struct ValidateApi {
    /// Request URL; `{{value}}` is substituted with the field value before sending.
    pub url: String,
    /// HTTP method: `"GET"` (default), `"POST"`, or `"HEAD"`.
    #[serde(default)]
    pub method: Option<String>,
    /// Extra headers as `[["name", "value"], …]`; values may contain `{{value}}`.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Request body for POST; `{{value}}` substitution allowed.
    #[serde(default)]
    pub body: Option<String>,
    /// If set, the response status must equal this exactly; otherwise any 2xx is accepted.
    #[serde(default)]
    pub expect_status: Option<u16>,
    /// Dotted JSON path (e.g. `"data.ok"`) whose value must be truthy in the response body.
    #[serde(default)]
    pub expect_json_path: Option<String>,
    /// Request timeout in milliseconds (default 5000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Error message shown to the user on validation failure.
    #[serde(default)]
    pub error: Option<String>,
}

impl ValidateApi {
    /// Substitute `{{value}}` verbatim — for headers and body where the raw
    /// value is required (e.g. an API key must not be percent-encoded).
    fn render(template: &str, value: &str) -> String {
        template.replace("{{value}}", value)
    }

    /// Substitute `{{value}}` with the value percent-encoded for a URL context.
    /// Only the value segment is encoded; the surrounding URL template is left
    /// as-is (the template author is responsible for the rest of the URL).
    /// Encodes everything outside RFC 3986 unreserved chars (A-Z a-z 0-9 - _ . ~).
    fn render_url(template: &str, value: &str) -> String {
        let encoded: String = value
            .bytes()
            .flat_map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                    vec![b as char]
                } else {
                    format!("%{b:02X}").chars().collect()
                }
            })
            .collect();
        template.replace("{{value}}", &encoded)
    }

    /// Perform the API validation request synchronously (blocking). Intended
    /// to be called from a `tokio::task::spawn_blocking` closure in the CLI.
    ///
    /// Returns `Ok(())` when the request passes all success criteria.
    /// Returns `Err(EngineError::InvalidInput { .. })` on any failure,
    /// using `self.error` as the message when set.
    pub fn call(&self, field_id: &str, value: &str) -> Result<()> {
        let fail = |reason: String| -> Result<()> {
            Err(EngineError::InvalidInput {
                field: field_id.to_string(),
                message: self.error.clone().unwrap_or(reason),
            })
        };

        // Security: refuse non-http(s) URLs.
        // Value is percent-encoded for the URL context only (not for headers/body).
        let url = Self::render_url(&self.url, value);
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return fail(format!(
                "api.url must start with http:// or https://, got: {url}"
            ));
        }
        // Refuse userinfo in the URL (token-exfil guard, mirrors processors_io).
        if let Some(authority) = url
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        {
            if authority.contains('@') {
                return fail(format!(
                    "api.url must not contain userinfo (user@host): {url}"
                ));
            }
        }

        let timeout_ms = self.timeout_ms.unwrap_or(5000);
        let method = self.method.as_deref().unwrap_or("GET").to_ascii_uppercase();

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_millis(timeout_ms)))
            // Disable auto-error on non-2xx so we can inspect the status ourselves.
            .http_status_as_error(false)
            // No redirects: validation endpoints respond directly, and following
            // redirects could replay secret-bearing headers to a different host.
            .max_redirects(0)
            .build()
            .into();

        // Rendered headers shared across all branches.
        let rendered_headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Self::render(v, value)))
            .collect();

        let map_err = |e: ureq::Error| EngineError::InvalidInput {
            field: field_id.to_string(),
            message: self
                .error
                .clone()
                .unwrap_or_else(|| format!("api validation request failed: {e}")),
        };

        // ureq 3: GET/HEAD → RequestBuilder<WithoutBody> (.call()), POST →
        // RequestBuilder<WithBody> (.send(body)). The two types cannot be
        // unified in a single `match`, so each branch is self-contained.
        let mut resp = match method.as_str() {
            "POST" => {
                let body = self
                    .body
                    .as_deref()
                    .map(|b| Self::render(b, value))
                    .unwrap_or_default();
                let mut req = agent.post(&url).header("User-Agent", "insmaller");
                for (k, v) in &rendered_headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.send(body.as_bytes()).map_err(map_err)?
            }
            "HEAD" => {
                let mut req = agent.head(&url).header("User-Agent", "insmaller");
                for (k, v) in &rendered_headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.call().map_err(map_err)?
            }
            _ => {
                let mut req = agent.get(&url).header("User-Agent", "insmaller");
                for (k, v) in &rendered_headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                req.call().map_err(map_err)?
            }
        };

        let status = resp.status().as_u16();

        // Status check.
        let status_ok = if let Some(expected) = self.expect_status {
            status == expected
        } else {
            (200..300).contains(&status)
        };
        if !status_ok {
            return fail(format!(
                "api validation failed: HTTP {status}{}",
                self.expect_status
                    .map(|e| format!(" (expected {e})"))
                    .unwrap_or_default()
            ));
        }

        // JSON-path check: parse body and walk the dotted path.
        if let Some(path) = &self.expect_json_path {
            let body_str =
                resp.body_mut()
                    .read_to_string()
                    .map_err(|e| EngineError::InvalidInput {
                        field: field_id.to_string(),
                        message: self
                            .error
                            .clone()
                            .unwrap_or_else(|| format!("api validation: failed to read body: {e}")),
                    })?;
            let json: serde_json::Value =
                serde_json::from_str(&body_str).map_err(|e| EngineError::InvalidInput {
                    field: field_id.to_string(),
                    message: self
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("api validation: response is not JSON: {e}")),
                })?;

            let resolved = resolve_json_path(&json, path);
            if !is_truthy(resolved) {
                return fail(format!(
                    "api validation failed: JSON path '{path}' is not truthy"
                ));
            }
        }

        Ok(())
    }
}

/// Walk a dotted JSON path (e.g. `"data.ok"` or `"items.0.ok"`) and return
/// the value at that location, or `None` if the path does not resolve.
/// Numeric segments index into arrays; string segments index into objects.
fn resolve_json_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for key in path.split('.') {
        cur = match cur {
            serde_json::Value::Array(arr) => {
                let idx = key.parse::<usize>().ok()?;
                arr.get(idx)?
            }
            other => other.get(key)?,
        };
    }
    Some(cur)
}

/// JSON value truthiness: `false`, `null`, `0`, `""`, `[]`, `{}` are falsy.
fn is_truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        None => false,
        Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
    }
}

/// Validation flags shared by `Field` and the catalog's `requires_input`
/// declarations. All optional; applied to the scalar string value of a
/// text/secret/path field (empties are handled by `required`).
///
/// Note: because this struct is `#[serde(flatten)]`-ed into `Field`, all
/// fields here (including `api`) appear as direct keys on `[[page.field]]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Validate {
    /// Regex the value must match in full (anchored automatically).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Named convenience validator.
    #[serde(default)]
    pub format: Option<FieldFormat>,
    #[serde(default)]
    pub min_length: Option<usize>,
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Numeric or string bounds. `Bound::Num` for numeric fields; `Bound::Str`
    /// for date/datetime fields (e.g. `min = "2026-06-01"`). Existing configs
    /// using bare numbers (`min = 5`) continue to work via `Bound::Num`.
    #[serde(default)]
    pub min: Option<Bound>,
    #[serde(default)]
    pub max: Option<Bound>,
    /// Custom message shown instead of the generated one.
    #[serde(default)]
    pub error: Option<String>,
    /// Optional API-call validation. Lives at the field level due to flatten.
    #[serde(default)]
    pub api: Option<ValidateApi>,
}

impl Validate {
    /// Validate a non-empty scalar value. `Ok(())` when it passes or when no
    /// rules are set. `field_id` is used for the error. Type-agnostic: handles
    /// numeric bounds only (date range handled by `check_typed`).
    pub fn check(&self, field_id: &str, value: &str) -> Result<()> {
        if value.is_empty() {
            return Ok(());
        }
        let fail = |reason: String| {
            Err(EngineError::InvalidInput {
                field: field_id.to_string(),
                message: self.error.clone().unwrap_or(reason),
            })
        };
        let len = value.chars().count();
        if let Some(min) = self.min_length {
            if len < min {
                return fail(format!("must be at least {min} character(s)"));
            }
        }
        if let Some(max) = self.max_length {
            if len > max {
                return fail(format!("must be at most {max} character(s)"));
            }
        }
        if let Some(fmt) = self.format {
            if !fmt.accepts(value) {
                return fail(format!("must be {}", fmt.label()));
            }
        }
        // Numeric bounds: only applied when the bound is Bound::Num (or when
        // any numeric bound exists). Bound::Str bounds are handled in check_typed.
        let num_min = self.min.as_ref().and_then(|b| {
            if let Bound::Num(n) = b {
                Some(*n)
            } else {
                None
            }
        });
        let num_max = self.max.as_ref().and_then(|b| {
            if let Bound::Num(n) = b {
                Some(*n)
            } else {
                None
            }
        });
        if num_min.is_some() || num_max.is_some() {
            match value.parse::<f64>() {
                // reject NaN/inf: NaN passes every bound (all comparisons false).
                Ok(n) if n.is_finite() => {
                    if let Some(min) = num_min {
                        if n < min {
                            return fail(format!("must be >= {min}"));
                        }
                    }
                    if let Some(max) = num_max {
                        if n > max {
                            return fail(format!("must be <= {max}"));
                        }
                    }
                }
                // non-finite Ok (NaN/inf) or parse error → not a usable number.
                _ => return fail("must be a number".into()),
            }
        }
        if let Some(pat) = &self.pattern {
            let re = regex::Regex::new(&format!("^(?:{pat})$")).map_err(|e| {
                EngineError::Config(format!("field '{field_id}' has an invalid pattern: {e}"))
            })?;
            if !re.is_match(value) {
                return fail(format!("must match {pat}"));
            }
        }
        Ok(())
    }

    /// Like `check` but also applies date/datetime well-formedness and range
    /// validation for `FieldType::Date` and `FieldType::Datetime`. For all
    /// other types, delegates to `check` unchanged.
    ///
    /// Call sites that know the field type should use this instead of `check`.
    pub fn check_typed(&self, field_type: FieldType, field_id: &str, value: &str) -> Result<()> {
        if value.is_empty() {
            return Ok(());
        }
        let fail = |reason: String| -> Result<()> {
            Err(EngineError::InvalidInput {
                field: field_id.to_string(),
                message: self.error.clone().unwrap_or(reason),
            })
        };
        match field_type {
            FieldType::Date => {
                let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                    EngineError::InvalidInput {
                        field: field_id.to_string(),
                        message: self.error.clone().unwrap_or_else(|| {
                            format!("must be a valid date in YYYY-MM-DD format, got: {value}")
                        }),
                    }
                })?;
                if let Some(Bound::Str(min_s)) = &self.min {
                    let min_d =
                        chrono::NaiveDate::parse_from_str(min_s, "%Y-%m-%d").map_err(|_| {
                            EngineError::Config(format!(
                                "field '{field_id}' has an invalid date min bound: {min_s}"
                            ))
                        })?;
                    if date < min_d {
                        return fail(format!("must be on or after {min_s}"));
                    }
                }
                if let Some(Bound::Str(max_s)) = &self.max {
                    let max_d =
                        chrono::NaiveDate::parse_from_str(max_s, "%Y-%m-%d").map_err(|_| {
                            EngineError::Config(format!(
                                "field '{field_id}' has an invalid date max bound: {max_s}"
                            ))
                        })?;
                    if date > max_d {
                        return fail(format!("must be on or before {max_s}"));
                    }
                }
                // Run only length/pattern checks — NOT numeric bounds or FieldFormat,
                // which don't apply to ISO date strings and produce misleading errors.
                self.check_text_only(field_id, value)
            }
            FieldType::Datetime => {
                let dt = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|_| EngineError::InvalidInput {
                        field: field_id.to_string(),
                        message: self.error.clone().unwrap_or_else(|| {
                            format!(
                                "must be a valid datetime in YYYY-MM-DDTHH:MM:SS format, got: {value}"
                            )
                        }),
                    })?;
                if let Some(Bound::Str(min_s)) = &self.min {
                    let min_dt = chrono::NaiveDateTime::parse_from_str(min_s, "%Y-%m-%dT%H:%M:%S")
                        .map_err(|_| {
                            EngineError::Config(format!(
                                "field '{field_id}' has an invalid datetime min bound: {min_s}"
                            ))
                        })?;
                    if dt < min_dt {
                        return fail(format!("must be on or after {min_s}"));
                    }
                }
                if let Some(Bound::Str(max_s)) = &self.max {
                    let max_dt = chrono::NaiveDateTime::parse_from_str(max_s, "%Y-%m-%dT%H:%M:%S")
                        .map_err(|_| {
                            EngineError::Config(format!(
                                "field '{field_id}' has an invalid datetime max bound: {max_s}"
                            ))
                        })?;
                    if dt > max_dt {
                        return fail(format!("must be on or before {max_s}"));
                    }
                }
                // Run only length/pattern checks — NOT numeric bounds or FieldFormat.
                self.check_text_only(field_id, value)
            }
            _ => self.check(field_id, value),
        }
    }

    /// Apply only min_length/max_length/pattern checks — no FieldFormat or
    /// numeric-bound logic. Used by Date/Datetime paths in `check_typed` so
    /// an ISO date string is never fed to the f64 or format validators.
    fn check_text_only(&self, field_id: &str, value: &str) -> Result<()> {
        if value.is_empty() {
            return Ok(());
        }
        let fail = |reason: String| {
            Err(EngineError::InvalidInput {
                field: field_id.to_string(),
                message: self.error.clone().unwrap_or(reason),
            })
        };
        let len = value.chars().count();
        if let Some(min) = self.min_length {
            if len < min {
                return fail(format!("must be at least {min} character(s)"));
            }
        }
        if let Some(max) = self.max_length {
            if len > max {
                return fail(format!("must be at most {max} character(s)"));
            }
        }
        if let Some(pat) = &self.pattern {
            let re = regex::Regex::new(&format!("^(?:{pat})$")).map_err(|e| {
                EngineError::Config(format!("field '{field_id}' has an invalid pattern: {e}"))
            })?;
            if !re.is_match(value) {
                return fail(format!("must match {pat}"));
            }
        }
        Ok(())
    }
}

/// Validate wizard schema constraints. Call after `WizardDef::from_str` to
/// catch semantic errors that TOML deserialization cannot detect.
///
/// Returns `Err(EngineError::Config(_))` on a hard violation.
/// Writes warnings to stderr for soft violations (non-fatal).
pub fn validate_wizard_schema(def: &WizardDef) -> Result<()> {
    for page in &def.pages {
        for field in &page.fields {
            validate_field_schema(field)?;
        }
    }
    Ok(())
}

fn validate_field_schema(field: &Field) -> Result<()> {
    // format= is meaningful only for free-text fields. Reject it on everything
    // else: on select types it runs against the option label and falsely rejects
    // valid options; on date/datetime/toggle it is semantically nonsensical.
    if let Some(fmt) = field.validate.format {
        let allowed = matches!(field.field_type, FieldType::Text | FieldType::Textarea | FieldType::Secret | FieldType::Path);
        if !allowed {
            return Err(EngineError::Config(format!(
                "field '{}': format={:?} is only valid on text/secret/path fields, not {:?}",
                field.id, fmt, field.field_type
            )));
        }
    }

    // Reject numeric Bound::Num on Date/Datetime fields: the bound must be an
    // ISO string (e.g. `min = "2026-06-01"`), not a bare number.
    if matches!(field.field_type, FieldType::Date | FieldType::Datetime) {
        if matches!(&field.validate.min, Some(Bound::Num(_))) {
            return Err(EngineError::Config(format!(
                "field '{}': min= on a {:?} field must be an ISO string (e.g. min = \"2026-06-01\"), not a number",
                field.id, field.field_type
            )));
        }
        if matches!(&field.validate.max, Some(Bound::Num(_))) {
            return Err(EngineError::Config(format!(
                "field '{}': max= on a {:?} field must be an ISO string (e.g. max = \"2027-12-31\"), not a number",
                field.id, field.field_type
            )));
        }
    }

    // Reject Bound::Str min/max on non-date fields: string bounds are only
    // meaningful as ISO dates. A quoted number like `min = "5"` on a text field
    // is silently ignored by the numeric-bound block, enforcing nothing.
    if !matches!(field.field_type, FieldType::Date | FieldType::Datetime) {
        if matches!(&field.validate.min, Some(Bound::Str(_))) {
            return Err(EngineError::Config(format!(
                "field '{}': min= is a quoted string but field type is {:?}; \
                 string bounds are only valid on date/datetime fields (use an unquoted number for numeric bounds)",
                field.id, field.field_type
            )));
        }
        if matches!(&field.validate.max, Some(Bound::Str(_))) {
            return Err(EngineError::Config(format!(
                "field '{}': max= is a quoted string but field type is {:?}; \
                 string bounds are only valid on date/datetime fields (use an unquoted number for numeric bounds)",
                field.id, field.field_type
            )));
        }
    }

    // Reject catalog.* source on Dropdown: dropdown answers are stored as
    // WizValue::Text and are never pushed to selected_keys, so a
    // catalog-sourced dropdown would silently install nothing.
    if field.field_type == FieldType::Dropdown {
        if let Some(src) = &field.source {
            if src.starts_with("catalog.") {
                return Err(EngineError::Config(format!(
                    "field '{}': source=\"{src}\" is not supported on type=dropdown \
                     (dropdown answers are not pushed to selected_keys; use single_select instead)",
                    field.id
                )));
            }
        }
    }

    if let Some(api) = &field.validate.api {
        // Hard reject: empty or non-http(s) url.
        if api.url.is_empty() {
            return Err(EngineError::Config(format!(
                "field '{}': api.url must not be empty",
                field.id
            )));
        }
        if !api.url.starts_with("http://") && !api.url.starts_with("https://") {
            return Err(EngineError::Config(format!(
                "field '{}': api.url must start with http:// or https://, got: {}",
                field.id, api.url
            )));
        }

        // Soft warn: api on a secret field without {{value}} in headers/body
        // (the value would never be sent — likely a config mistake).
        if field.field_type == FieldType::Secret {
            let has_value_in_headers = api.headers.iter().any(|(_, v)| v.contains("{{value}}"));
            let has_value_in_body = api
                .body
                .as_deref()
                .map(|b| b.contains("{{value}}"))
                .unwrap_or(false);
            let has_value_in_url = api.url.contains("{{value}}");
            if !has_value_in_headers && !has_value_in_body && !has_value_in_url {
                eprintln!(
                    "warning: field '{}' is type=secret with api validation but {{{{value}}}} \
                     is not used in api.url, api.headers, or api.body — the secret will not \
                     be sent to the API",
                    field.id
                );
            }
        }
    }

    Ok(())
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
    /// Value validators (text/secret/path fields). Flattened: all Validate
    /// fields appear as direct keys on `[[page.field]]`.
    #[serde(flatten)]
    pub validate: Validate,
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
    #[serde(flatten)]
    pub validate: Validate,
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
            validate: self.validate.clone(),
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
    /// Catalog `group`/`category` this option belongs to, if any. Drives the
    /// collapsible group headers in the TUI; `None` ⇒ rendered ungrouped.
    pub group: Option<String>,
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
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
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

/// Evaluate a condition against collected vars. Mirrors the reference installer, plus
/// version-compare operators (`>= <= > <`, and semver-aware `== !=`) — the
/// single expression grammar reused by entries, item conditions, and pages.
pub fn eval_condition(expr: &str, vars: &Map<String, Value>) -> bool {
    use std::cmp::Ordering;
    let s = expr.trim();
    let get = |name: &str| -> String { vars.get(name.trim()).map(var_as_str).unwrap_or_default() };
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
                    group: o.group.clone(),
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
            group: None,
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
                    if let Value::String(s) = &stored {
                        let label = synthetic.prompt.as_deref().unwrap_or(&synthetic.id);
                        synthetic.validate.check_typed(synthetic.field_type, label, s)?;
                    }
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
            if let Value::String(s) = &stored {
                let label = field.prompt.as_deref().unwrap_or(&field.id);
                field.validate.check_typed(field.field_type, label, s)?;
            }
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
        if page
            .condition
            .as_deref()
            .is_some_and(|c| !eval_condition(c, vars))
        {
            continue;
        }
        for field in &page.fields {
            if field
                .condition
                .as_deref()
                .is_some_and(|c| !eval_condition(c, vars))
            {
                continue;
            }
            if !is_catalog_source(field) {
                continue;
            }
            match vars.get(&field.id) {
                Some(Value::Array(a)) => out
                    .selected_keys
                    .extend(a.iter().filter_map(|v| v.as_str().map(String::from))),
                Some(Value::String(s)) if !s.is_empty() => out.selected_keys.push(s.clone()),
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
        let total = (0..self.def.pages.len())
            .filter(|&i| self.active(i))
            .count();
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
                    if let Value::String(s) = v {
                        let label = f.prompt.as_deref().unwrap_or(&f.id);
                        f.validate.check_typed(f.field_type, label, s)?;
                    }
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
            &serde_json::json!({"CODING":"aider"})
                .as_object()
                .unwrap()
                .clone()
        ));
        assert!(eval_condition(
            "${CODING} in 'claude,aider'",
            &serde_json::json!({"CODING":"aider"})
                .as_object()
                .unwrap()
                .clone()
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
        a.insert(
            "INSTALL_TOOLS".into(),
            serde_json::json!(["ripgrep", "node"]),
        );
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
            validate: Validate::default(),
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
            validate: Validate::default(),
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

    #[test]
    fn validate_format_and_bounds() {
        let v = Validate {
            format: Some(FieldFormat::Integer),
            ..Default::default()
        };
        assert!(v.check("PORT", "8080").is_ok());
        assert!(v.check("PORT", "8a").is_err());
        assert!(v.check("PORT", "").is_ok()); // empties are `required`'s job

        let r = Validate {
            format: Some(FieldFormat::Integer),
            min: Some(Bound::Num(1.0)),
            max: Some(Bound::Num(65535.0)),
            ..Default::default()
        };
        assert!(r.check("PORT", "443").is_ok());
        assert!(r.check("PORT", "0").is_err());
        assert!(r.check("PORT", "70000").is_err());

        assert!(Validate {
            format: Some(FieldFormat::Alpha),
            ..Default::default()
        }
        .check("N", "abcZ")
        .is_ok());
        assert!(Validate {
            format: Some(FieldFormat::Alpha),
            ..Default::default()
        }
        .check("N", "ab1")
        .is_err());
        assert!(Validate {
            format: Some(FieldFormat::Email),
            ..Default::default()
        }
        .check("E", "a@b.co")
        .is_ok());
        assert!(Validate {
            format: Some(FieldFormat::Email),
            ..Default::default()
        }
        .check("E", "nope")
        .is_err());
    }

    #[test]
    fn validate_rejects_nan_and_inf() {
        // NaN compares false to every bound, so it must be rejected outright,
        // both as a `number` format and under min/max bounds.
        let num = Validate {
            format: Some(FieldFormat::Number),
            ..Default::default()
        };
        assert!(num.check("X", "nan").is_err());
        assert!(num.check("X", "inf").is_err());
        assert!(num.check("X", "3.14").is_ok());

        let bounded = Validate {
            min: Some(Bound::Num(1.0)),
            max: Some(Bound::Num(10.0)),
            ..Default::default()
        };
        assert!(
            bounded.check("X", "nan").is_err(),
            "NaN must not slip past bounds"
        );
        assert!(bounded.check("X", "inf").is_err());
        assert!(bounded.check("X", "5").is_ok());
    }

    #[test]
    fn validate_pattern_is_full_match_and_length() {
        let v = Validate {
            pattern: Some("[a-z]+".into()),
            ..Default::default()
        };
        assert!(v.check("S", "abc").is_ok());
        assert!(
            v.check("S", "abc1").is_err(),
            "anchored: trailing digit rejected"
        );

        let l = Validate {
            max_length: Some(3),
            ..Default::default()
        };
        assert!(l.check("S", "abc").is_ok());
        assert!(l.check("S", "abcd").is_err());
    }

    #[test]
    fn validate_custom_error_message() {
        let v = Validate {
            format: Some(FieldFormat::Integer),
            error: Some("ports are numbers".into()),
            ..Default::default()
        };
        assert!(format!("{}", v.check("PORT", "x").unwrap_err()).contains("ports are numbers"));
    }

    #[test]
    fn invalid_pattern_is_config_error() {
        let v = Validate {
            pattern: Some("(".into()),
            ..Default::default()
        };
        assert!(matches!(v.check("S", "x"), Err(EngineError::Config(_))));
    }

    #[test]
    fn run_wizard_rejects_invalid_answer() {
        let wiz = WizardDef::from_str(
            "[[page]]\nid = \"p\"\n[[page.field]]\nid = \"PORT\"\ntype = \"text\"\nformat = \"integer\"\n",
        )
        .unwrap();
        let cat = Catalog::default();
        let mut m = Map::new();
        m.insert("PORT".into(), Value::String("abc".into()));
        let r = run_wizard(&wiz, &cat, &StaticAnswerer(m), &[]);
        assert!(matches!(r, Err(EngineError::InvalidInput { .. })));
    }

    // ── new FieldType variants ───────────────────────────────────────────────

    #[test]
    fn new_field_types_deserialize() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "country"
            type = "dropdown"
            [[page.field]]
            id = "notes"
            type = "textarea"
            [[page.field]]
            id = "go_live"
            type = "date"
            [[page.field]]
            id = "launch_at"
            type = "datetime"
        "#,
        )
        .unwrap();
        let fields = &wiz.pages[0].fields;
        assert_eq!(fields[0].field_type, FieldType::Dropdown);
        assert_eq!(fields[1].field_type, FieldType::Textarea);
        assert_eq!(fields[2].field_type, FieldType::Date);
        assert_eq!(fields[3].field_type, FieldType::Datetime);
    }

    // ── Bound untagged enum ──────────────────────────────────────────────────

    #[test]
    fn bound_num_parses_from_toml() {
        #[derive(Deserialize)]
        struct T {
            min: Bound,
            max: Bound,
        }
        let t: T = toml::from_str("min = 5\nmax = 65535.0\n").unwrap();
        assert_eq!(t.min, Bound::Num(5.0));
        assert_eq!(t.max, Bound::Num(65535.0));
    }

    #[test]
    fn bound_str_parses_from_toml() {
        #[derive(Deserialize)]
        struct T {
            min: Bound,
            max: Bound,
        }
        let t: T = toml::from_str(
            r#"min = "2026-06-01"
max = "2027-12-31"
"#,
        )
        .unwrap();
        assert_eq!(t.min, Bound::Str("2026-06-01".into()));
        assert_eq!(t.max, Bound::Str("2027-12-31".into()));
    }

    #[test]
    fn existing_numeric_min_max_configs_still_work() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "PORT"
            type = "text"
            format = "integer"
            min = 1
            max = 65535
        "#,
        )
        .unwrap();
        let v = &wiz.pages[0].fields[0].validate;
        assert!(matches!(v.min, Some(Bound::Num(n)) if n == 1.0));
        assert!(matches!(v.max, Some(Bound::Num(n)) if n == 65535.0));
        assert!(v.check("PORT", "443").is_ok());
        assert!(v.check("PORT", "0").is_err());
    }

    // ── Date validation ──────────────────────────────────────────────────────

    #[test]
    fn date_valid_passes() {
        let v = Validate::default();
        assert!(v.check_typed(FieldType::Date, "D", "2026-06-01").is_ok());
    }

    #[test]
    fn date_malformed_rejected() {
        let v = Validate::default();
        assert!(v.check_typed(FieldType::Date, "D", "not-a-date").is_err());
        assert!(v.check_typed(FieldType::Date, "D", "2026-13-01").is_err());
        assert!(v.check_typed(FieldType::Date, "D", "2026/06/01").is_err());
    }

    #[test]
    fn date_range_enforced() {
        let v = Validate {
            min: Some(Bound::Str("2026-06-01".into())),
            max: Some(Bound::Str("2027-12-31".into())),
            ..Default::default()
        };
        assert!(v.check_typed(FieldType::Date, "D", "2026-06-01").is_ok());
        assert!(v.check_typed(FieldType::Date, "D", "2027-12-31").is_ok());
        assert!(v.check_typed(FieldType::Date, "D", "2026-05-31").is_err());
        assert!(v.check_typed(FieldType::Date, "D", "2028-01-01").is_err());
    }

    #[test]
    fn date_empty_passes() {
        let v = Validate {
            min: Some(Bound::Str("2026-06-01".into())),
            ..Default::default()
        };
        // empty is `required`'s job, not validate's
        assert!(v.check_typed(FieldType::Date, "D", "").is_ok());
    }

    // ── Datetime validation ──────────────────────────────────────────────────

    #[test]
    fn datetime_valid_passes() {
        let v = Validate::default();
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "2026-06-01T10:00:00")
            .is_ok());
    }

    #[test]
    fn datetime_malformed_rejected() {
        let v = Validate::default();
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "2026-06-01")
            .is_err());
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "not-a-datetime")
            .is_err());
    }

    #[test]
    fn datetime_range_enforced() {
        let v = Validate {
            min: Some(Bound::Str("2026-06-01T00:00:00".into())),
            max: Some(Bound::Str("2026-12-31T23:59:59".into())),
            ..Default::default()
        };
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "2026-06-01T00:00:00")
            .is_ok());
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "2025-12-31T23:59:59")
            .is_err());
        assert!(v
            .check_typed(FieldType::Datetime, "DT", "2027-01-01T00:00:00")
            .is_err());
    }

    // ── ValidateApi deserialization ──────────────────────────────────────────

    #[test]
    fn validate_api_deserializes_from_field_level_toml() {
        // [page.field.api] because Validate is flattened into Field.
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "KEY"
            type = "secret"
            [page.field.api]
            url = "https://api.example.com/check"
            method = "POST"
            headers = [["x-api-key", "{{value}}"]]
            body = '{"test":true}'
            expect_status = 200
            expect_json_path = "data.ok"
            timeout_ms = 3000
            error = "Bad key"
        "#,
        )
        .unwrap();
        let api = wiz.pages[0].fields[0].validate.api.as_ref().unwrap();
        assert_eq!(api.url, "https://api.example.com/check");
        assert_eq!(api.method.as_deref(), Some("POST"));
        assert_eq!(api.headers, vec![("x-api-key".into(), "{{value}}".into())]);
        assert_eq!(api.expect_status, Some(200));
        assert_eq!(api.expect_json_path.as_deref(), Some("data.ok"));
        assert_eq!(api.timeout_ms, Some(3000));
        assert_eq!(api.error.as_deref(), Some("Bad key"));
    }

    #[test]
    fn validate_api_defaults() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "K"
            type = "text"
            [page.field.api]
            url = "https://api.example.com/v"
        "#,
        )
        .unwrap();
        let api = wiz.pages[0].fields[0].validate.api.as_ref().unwrap();
        assert!(api.method.is_none());
        assert!(api.headers.is_empty());
        assert!(api.body.is_none());
        assert!(api.expect_status.is_none());
        assert!(api.expect_json_path.is_none());
        assert!(api.timeout_ms.is_none());
        assert!(api.error.is_none());
    }

    // ── Schema validator ─────────────────────────────────────────────────────

    #[test]
    fn schema_rejects_empty_api_url() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "K"
            type = "text"
            [page.field.api]
            url = ""
        "#,
        )
        .unwrap();
        assert!(matches!(
            validate_wizard_schema(&wiz),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn schema_rejects_non_http_api_url() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "K"
            type = "text"
            [page.field.api]
            url = "ftp://example.com/check"
        "#,
        )
        .unwrap();
        assert!(matches!(
            validate_wizard_schema(&wiz),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn schema_rejects_format_on_dropdown() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "K"
            type = "dropdown"
            format = "integer"
            options = ["1","2"]
        "#,
        )
        .unwrap();
        assert!(matches!(
            validate_wizard_schema(&wiz),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn schema_rejects_format_on_date() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "D"
            type = "date"
            format = "integer"
        "#,
        )
        .unwrap();
        assert!(matches!(
            validate_wizard_schema(&wiz),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn schema_accepts_valid_wizard() {
        let wiz = WizardDef::from_str(
            r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "K"
            type = "text"
            format = "email"
            [page.field.api]
            url = "https://api.example.com/check"
        "#,
        )
        .unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    // ── API validation call (offline: localhost ephemeral server) ─────────────

    fn start_http_server(response: &'static str) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the whole request (headers + any body) until the client
                // pauses to wait for our response. Responding and closing before
                // the client has finished sending its body resets the connection
                // on Windows, so ureq reports a transport error and the POST-body
                // test flakes. A short read timeout ends the loop once the client
                // stops sending.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                let mut tmp = [0u8; 1024];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,        // client closed
                        Ok(_) => continue,     // more request bytes
                        Err(_) => break,       // timeout: client now awaiting response
                    }
                }
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    #[test]
    fn api_call_success_2xx() {
        let port = start_http_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nok",
        );
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "somevalue").is_ok());
    }

    #[test]
    fn api_call_expect_status_match() {
        let port = start_http_server("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: Some(401),
            expect_json_path: None,
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "somevalue").is_ok());
    }

    #[test]
    fn api_call_expect_status_mismatch_fails() {
        let port = start_http_server("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: Some(200),
            expect_json_path: None,
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(matches!(
            api.call("field", "somevalue"),
            Err(EngineError::InvalidInput { .. })
        ));
    }

    #[test]
    fn api_call_json_path_truthy_passes() {
        let port = start_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"data\":{\"ok\":true}}",
        );
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: Some("data.ok".into()),
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "somevalue").is_ok());
    }

    #[test]
    fn api_call_json_path_falsy_fails() {
        let port = start_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 21\r\n\r\n{\"data\":{\"ok\":false}}",
        );
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: Some("data.ok".into()),
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(matches!(
            api.call("field", "somevalue"),
            Err(EngineError::InvalidInput { .. })
        ));
    }

    #[test]
    fn api_call_value_substituted_in_url() {
        // Server just returns 200; we check the URL was rendered (can't
        // inspect what ureq sent, but a wrong port → error which would fail).
        let port = start_http_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check/{{{{value}}}}"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "myval").is_ok());
    }

    #[test]
    fn api_call_non_http_url_rejected() {
        let api = ValidateApi {
            url: "ftp://example.com/check".into(),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(100),
            error: None,
        };
        assert!(matches!(
            api.call("field", "val"),
            Err(EngineError::InvalidInput { .. })
        ));
    }

    #[test]
    fn api_call_timeout_is_error() {
        // Connect to a port that will never respond. Use a port the OS is
        // unlikely to have bound. Even if the OS refuses the connection
        // immediately, ureq will return an error which maps to InvalidInput.
        let api = ValidateApi {
            url: "http://127.0.0.1:19999/check".into(),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(200),
            error: None,
        };
        assert!(matches!(
            api.call("field", "val"),
            Err(EngineError::InvalidInput { .. })
        ));
    }

    #[test]
    fn api_call_post_with_body() {
        let port = start_http_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: Some("POST".into()),
            headers: vec![("content-type".into(), "application/json".into())],
            body: Some("{\"key\":\"{{value}}\"}".into()),
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "mykey").is_ok());
    }

    // ── JSON path + is_truthy helpers ────────────────────────────────────────

    #[test]
    fn json_path_resolves_nested() {
        let v = serde_json::json!({"a": {"b": {"c": 42}}});
        assert_eq!(
            resolve_json_path(&v, "a.b.c"),
            Some(&serde_json::Value::Number(42.into()))
        );
        assert!(resolve_json_path(&v, "a.b.x").is_none());
    }

    #[test]
    fn json_path_indexes_arrays() {
        let v = serde_json::json!({"items": [{"ok": true}, {"ok": false}]});
        // numeric segment indexes into the array
        assert_eq!(
            resolve_json_path(&v, "items.0.ok"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            resolve_json_path(&v, "items.1.ok"),
            Some(&serde_json::Value::Bool(false))
        );
        // out-of-bounds → None
        assert!(resolve_json_path(&v, "items.2.ok").is_none());
        // non-numeric key on an array → None
        assert!(resolve_json_path(&v, "items.x").is_none());
    }

    #[test]
    fn api_call_json_path_array_indexed_truthy() {
        let port = start_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"items\":[{\"ok\":true}]}",
        );
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: Some("items.0.ok".into()),
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(api.call("field", "somevalue").is_ok());
    }

    #[test]
    fn api_call_json_path_array_indexed_falsy() {
        let port = start_http_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 24\r\n\r\n{\"items\":[{\"ok\":false}]}",
        );
        let api = ValidateApi {
            url: format!("http://127.0.0.1:{port}/check"),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: Some("items.0.ok".into()),
            timeout_ms: Some(2000),
            error: None,
        };
        assert!(matches!(
            api.call("field", "somevalue"),
            Err(EngineError::InvalidInput { .. })
        ));
    }

    #[test]
    fn is_truthy_values() {
        assert!(is_truthy(Some(&serde_json::Value::Bool(true))));
        assert!(!is_truthy(Some(&serde_json::Value::Bool(false))));
        assert!(!is_truthy(Some(&serde_json::Value::Null)));
        assert!(is_truthy(Some(&serde_json::Value::String("x".into()))));
        assert!(!is_truthy(Some(&serde_json::Value::String("".into()))));
        assert!(!is_truthy(None));
    }

    // ── Fix 2: date/datetime check_typed must not apply numeric/format checks ─

    #[test]
    fn date_with_numeric_bound_in_validate_does_not_produce_must_be_a_number() {
        // A config mistake: numeric Bound::Num on a date field. The schema
        // validator rejects this at load time, but check_typed must not panic
        // or emit "must be a number" — it simply skips the numeric path.
        let v = Validate {
            min: Some(Bound::Num(5.0)),
            ..Default::default()
        };
        // Should not return "must be a number" for a valid ISO date string.
        // (Bound::Num is skipped for Date fields; schema validator catches this.)
        let result = v.check_typed(FieldType::Date, "D", "2026-06-01");
        assert!(result.is_ok(), "numeric Bound::Num must not trigger on date: {result:?}");
    }

    #[test]
    fn date_with_format_integer_in_validate_does_not_misfire() {
        // format=integer on a date field is rejected by schema validator, but
        // check_typed must not apply FieldFormat to the ISO string.
        let v = Validate {
            format: Some(FieldFormat::Integer),
            ..Default::default()
        };
        let result = v.check_typed(FieldType::Date, "D", "2026-06-01");
        assert!(result.is_ok(), "FieldFormat must not apply in date check_typed: {result:?}");
    }

    // ── Fix 3: schema validator new rejections ────────────────────────────────

    #[test]
    fn schema_rejects_numeric_min_on_date_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "D"
            type = "date"
            min = 5
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_rejects_numeric_max_on_datetime_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "DT"
            type = "datetime"
            max = 9999
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_accepts_string_min_max_on_date_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "D"
            type = "date"
            min = "2026-06-01"
            max = "2027-12-31"
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    #[test]
    fn schema_rejects_catalog_source_on_dropdown() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "TOOLS"
            type = "dropdown"
            source = "catalog.tools"
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_accepts_static_options_on_dropdown() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "COUNTRY"
            type = "dropdown"
            options = ["US", "PH", "DE"]
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    // ── Fix 1 (extended): format rejected on single_select / multiselect ────────

    #[test]
    fn schema_rejects_format_on_single_select() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "ENV"
            type = "single_select"
            format = "email"
            options = ["Production", "Staging"]
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_rejects_format_on_multiselect() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "TOOLS"
            type = "multiselect"
            format = "integer"
            options = ["node", "ripgrep"]
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_accepts_format_on_text_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "PORT"
            type = "text"
            format = "integer"
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    #[test]
    fn schema_accepts_format_on_textarea() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "NOTES"
            type = "textarea"
            format = "integer"
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    // ── Fix 2: Bound::Str on non-date fields is rejected ─────────────────────

    #[test]
    fn schema_rejects_string_min_on_text_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "PORT"
            type = "text"
            min = "5"
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_rejects_string_max_on_secret_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "KEY"
            type = "secret"
            max = "100"
        "#).unwrap();
        assert!(matches!(validate_wizard_schema(&wiz), Err(EngineError::Config(_))));
    }

    #[test]
    fn schema_accepts_numeric_min_on_text_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "PORT"
            type = "text"
            min = 1
            max = 65535
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    #[test]
    fn schema_accepts_string_min_on_date_field() {
        let wiz = WizardDef::from_str(r#"
            [[page]]
            id = "p"
            [[page.field]]
            id = "D"
            type = "date"
            min = "2026-01-01"
        "#).unwrap();
        assert!(validate_wizard_schema(&wiz).is_ok());
    }

    // ── Fix 4: URL percent-encoding of {{value}} ──────────────────────────────

    #[test]
    fn render_url_encodes_special_chars() {
        // / ? # @ and spaces must be encoded; unreserved chars must not be.
        let encoded = ValidateApi::render_url("https://example.com/check/{{value}}", "a/b?c#d@e f");
        assert_eq!(encoded, "https://example.com/check/a%2Fb%3Fc%23d%40e%20f");
    }

    #[test]
    fn render_url_leaves_unreserved_intact() {
        let encoded = ValidateApi::render_url("https://example.com/{{value}}", "abc-XYZ_1.2~");
        assert_eq!(encoded, "https://example.com/abc-XYZ_1.2~");
    }

    #[test]
    fn render_leaves_headers_raw() {
        // headers/body use render(), not render_url() — value must stay verbatim.
        let raw = ValidateApi::render("Bearer {{value}}", "sk-abc/def?x=1");
        assert_eq!(raw, "Bearer sk-abc/def?x=1");
    }

    #[test]
    fn api_call_non_http_url_still_rejected_after_encoding() {
        // A value that encodes to something benign; the scheme is still ftp.
        let api = ValidateApi {
            url: "ftp://example.com/{{value}}".into(),
            method: None,
            headers: vec![],
            body: None,
            expect_status: None,
            expect_json_path: None,
            timeout_ms: Some(100),
            error: None,
        };
        assert!(matches!(
            api.call("field", "val"),
            Err(EngineError::InvalidInput { .. })
        ));
    }
}
