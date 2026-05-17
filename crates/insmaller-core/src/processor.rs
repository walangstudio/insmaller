use crate::ctx::Ctx;
use crate::input::InputResolver;
use crate::reporter::Reporter;
use crate::step::Step;
use serde_json::{Map, Value};

/// What a processor hands back. `register` is merged flat into the pipeline's
/// local var map; `value` is bound to the step's `register_as` name (if any);
/// `skipped` means the step intentionally did nothing (e.g. optional input not
/// provided) — it must not be treated as a failure and any var it would have
/// produced stays absent (see `Step::requires`). `Default` == the old `()`.
#[derive(Debug, Clone, Default)]
pub struct StepOutput {
    pub register: Map<String, Value>,
    pub value: Option<Value>,
    pub skipped: bool,
}

impl StepOutput {
    pub fn ok() -> Self {
        Self::default()
    }
    pub fn skipped() -> Self {
        Self {
            skipped: true,
            ..Self::default()
        }
    }
    pub fn value(v: impl Into<Value>) -> Self {
        Self {
            value: Some(v.into()),
            ..Self::default()
        }
    }
}

/// A processor executes one step kind. Implementations are the only place
/// real side effects (spawn, fs, network) happen. Params come pre-validated
/// as the step's TOML table; the processor renders any templated strings
/// against `ctx` itself (it knows which of its fields are templated).
#[async_trait::async_trait]
pub trait Processor: Send + Sync {
    /// The `type` value this processor handles (e.g. "shell", "exec").
    fn kind(&self) -> &str;

    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        reporter: &dyn Reporter,
        input: &dyn InputResolver,
    ) -> anyhow::Result<StepOutput>;
}
