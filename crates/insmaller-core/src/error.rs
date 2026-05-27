use thiserror::Error;

/// Engine-level typed errors. Processors and the orchestrator use `anyhow`
/// internally (mirrors the reference installer's convention); this enum is for the
/// boundaries where callers need to branch on the failure kind.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("config: {0}")]
    Config(String),

    #[error("unknown processor type '{0}'")]
    UnknownProcessor(String),

    #[error("unknown recipe '{0}'")]
    UnknownRecipe(String),

    #[error("no desugar rule for spec '{0}'")]
    NoDesugar(String),

    /// A terse spec was malformed (user input error, distinct from a broken
    /// engine config).
    #[error("{0}")]
    BadSpec(String),

    #[error("entry '{0}' not found in catalog")]
    NotFound(String),

    #[error("dependency cycle detected involving '{0}'")]
    Cycle(String),

    #[error("step '{step}' of '{key}' failed: {msg}")]
    StepFailed {
        step: String,
        key: String,
        msg: String,
    },

    #[error("installing dep '{dep}' of '{key}': {msg}")]
    DepFailed {
        dep: String,
        key: String,
        msg: String,
    },

    #[error("post_install of '{key}': {cmd}: {msg}")]
    PostInstall {
        key: String,
        cmd: String,
        msg: String,
    },

    #[error("verify of '{key}': {msg}")]
    Verify { key: String, msg: String },

    #[error("required input '{0}' is not available (non-interactive context)")]
    MissingInput(String),

    #[error("template render failed for {what}: {source}")]
    Render {
        what: String,
        #[source]
        source: minijinja::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wraps anyhow from the verbatim-ported helpers (expand_home, etc.).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;
