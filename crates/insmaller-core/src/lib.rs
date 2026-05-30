//! insmaller-core — config-driven installer engine.
//!
//! NOTE: crate is `insmaller-core`, not `installer-core`. Windows' Installer
//! Detection heuristic forces a UAC elevation prompt for any executable whose
//! name contains "install"/"setup"/"update" — which broke `cargo test` (os
//! error 740). Keep "install" out of crate/bin names.
//!
//! Declarative step pipelines + pluggable processors. A single TOML engine
//! config defines processors, recipes, lifecycle and settings; packages live
//! in a separate host-supplied source (see [`EntrySource`], added in B5).
//!
//! Build order: B1 core model (this) → B2 config/desugar → B3 processors →
//! B4 orchestrator/sentinel → B5 EntrySource + e2e.

pub mod config;
pub mod ctx;
pub mod desugar;
pub mod error;
pub mod input;
pub mod json_catalog;
pub mod orchestrator;
pub mod pathenv;
pub mod plugin;
pub mod processor;
pub mod processors;
pub mod processors_io;
pub mod registry;
pub mod reporter;
pub mod scripts;
pub mod sentinel;
pub mod step;
pub mod tasks;
pub mod wizard;

pub use config::{
    peek_dispatch_settings, CompiledTask, EngineConfig, LoadedConfig, OutputFormat, ParseKind,
    ProjectMeta, Recipe, SentinelScope, Settings, SetupOutput, TaskDef, ThemeColors,
};
pub use ctx::Ctx;
pub use desugar::{desugar, Desugared};
pub use error::{EngineError, Result};
pub use input::{
    env_nonempty, EnvResolver, InputResolver, PromptSpec, ResolvedInput, StaticResolver,
};
pub use json_catalog::{Catalog, CatalogOption};
pub use wizard::{
    choices_for_vars, collect_outcome, eval_condition, run_wizard, Answerer, Choice, Field,
    FieldType, InputDecl, Page, StaticAnswerer, WizValue, WizardDef, WizardOutcome,
    WizardSession, SELECTED_INPUTS,
};
pub use orchestrator::{
    install_many, install_many_with, run_step_pipeline, uninstall_many, uninstall_many_with,
    EntryRef, EntrySource, InstallSummary, RunOpts,
};
pub use processors_io::write_setup_output;
pub use tasks::{run_task, run_tasks};
pub use plugin::{
    register_external, ExternalProcessor, PluginResponse, PluginTransport, PROTOCOL,
};
pub use processor::{Processor, StepOutput};
pub use processors::builtins;
pub use registry::ProcessorRegistry;
pub use reporter::{JsonReporter, NullReporter, Reporter, StdoutReporter};
pub use sentinel::{Sentinel, SentinelData};
pub use step::Step;
