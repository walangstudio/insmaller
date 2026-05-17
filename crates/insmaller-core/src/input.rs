use std::collections::HashMap;

/// What a `prompt` / `save_input` step is asking for.
#[derive(Debug, Clone)]
pub struct PromptSpec {
    /// Env var the value is sourced from / saved as.
    pub env_key: String,
    pub message: String,
    pub required: bool,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedInput {
    Value(String),
    /// Not provided, not required — skip the step that needed it.
    Skip,
    /// Not provided but required — caller must fail fast (never block).
    Fail(String),
}

/// The keystone seam. `prompt`/`save_input` resolve through this and NEVER
/// touch stdin directly, so the unattended container path is structurally
/// incapable of hanging on a prompt.
pub trait InputResolver: Send + Sync {
    fn resolve(&self, key: &str, spec: &PromptSpec) -> ResolvedInput;
}

/// Container / non-interactive resolver: reads the environment only. A
/// missing required value fails fast into the install summary; it can
/// never block. This is the contract that keeps `entrypoint` safe.
pub struct EnvResolver;

impl InputResolver for EnvResolver {
    fn resolve(&self, _key: &str, spec: &PromptSpec) -> ResolvedInput {
        match std::env::var(&spec.env_key) {
            Ok(v) if !v.is_empty() => ResolvedInput::Value(v),
            _ if spec.required => ResolvedInput::Fail(format!(
                "input '{}' required but not set in environment (non-interactive context)",
                spec.env_key
            )),
            _ => ResolvedInput::Skip,
        }
    }
}

/// Test/double resolver backed by an in-memory map.
pub struct StaticResolver(pub HashMap<String, String>);

impl InputResolver for StaticResolver {
    fn resolve(&self, _key: &str, spec: &PromptSpec) -> ResolvedInput {
        match self.0.get(&spec.env_key) {
            Some(v) if !v.is_empty() => ResolvedInput::Value(v.clone()),
            _ if spec.required => ResolvedInput::Fail(format!("missing '{}'", spec.env_key)),
            _ => ResolvedInput::Skip,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_resolver_missing_required_fails_fast_not_blocks() {
        let r = EnvResolver;
        let spec = PromptSpec {
            env_key: "INSMALLER_DEFINITELY_UNSET_XYZ".into(),
            message: "x".into(),
            required: true,
            secret: false,
        };
        // The point: returns immediately with Fail, never reads stdin.
        assert!(matches!(r.resolve("k", &spec), ResolvedInput::Fail(_)));
    }

    #[test]
    fn env_resolver_missing_optional_skips() {
        let r = EnvResolver;
        let spec = PromptSpec {
            env_key: "INSMALLER_DEFINITELY_UNSET_XYZ".into(),
            message: "x".into(),
            required: false,
            secret: false,
        };
        assert_eq!(r.resolve("k", &spec), ResolvedInput::Skip);
    }

    #[test]
    fn static_resolver_returns_value() {
        let mut m = HashMap::new();
        m.insert("TOKEN".to_string(), "abc".to_string());
        let r = StaticResolver(m);
        let spec = PromptSpec {
            env_key: "TOKEN".into(),
            message: "x".into(),
            required: true,
            secret: true,
        };
        assert_eq!(r.resolve("k", &spec), ResolvedInput::Value("abc".into()));
    }
}
