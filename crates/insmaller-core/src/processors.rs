//! The six built-in processors. Side effects (spawn/fs) go through the
//! verbatim-ported pathenv helpers. Each processor keeps its templated-field
//! rendering and arg-building in a pure helper so tests need no subprocess.

use crate::ctx::Ctx;
use crate::input::InputResolver;
use crate::pathenv::{enriched_path, expand_home, resolve_in_path, run_cmd, run_sh};
use crate::processor::{Processor, StepOutput};
use crate::reporter::Reporter;
use crate::registry::ProcessorRegistry;
use crate::scripts;
use crate::step::Step;
use anyhow::{bail, Context as _, Result};
use std::sync::Arc;

type Globs = Arc<Vec<String>>;

/// Register all built-ins from engine `[settings]` (path globs + the
/// download hardening knobs).
pub fn builtins(settings: &crate::config::Settings) -> ProcessorRegistry {
    let g: Globs = Arc::new(settings.path_globs.clone());
    let mut r = ProcessorRegistry::new();
    r.register(Arc::new(ShellProcessor(g.clone())));
    r.register(Arc::new(ExecProcessor(g.clone())));
    r.register(Arc::new(MergeJsonProcessor(g.clone())));
    r.register(Arc::new(CheckCommandProcessor(g.clone())));
    r.register(Arc::new(SentinelMetaProcessor));
    crate::processors_io::register(&mut r, g, settings);
    r
}

// ── shell ──────────────────────────────────────────────────────────────────

pub struct ShellProcessor(Globs);

/// Resolve a shell step's script: inline `script`, or `script_file`
/// (embedded verbatim script, else a real file), then render against ctx.
fn resolve_script(step: &Step, ctx: &Ctx) -> Result<String> {
    let raw = if let Some(s) = step.param_str("script") {
        s.to_string()
    } else if let Some(f) = step.param_str("script_file") {
        match scripts::embedded(f) {
            Some(s) => s.to_string(),
            None => std::fs::read_to_string(f)
                .with_context(|| format!("script_file '{f}' not embedded and not readable"))?,
        }
    } else {
        bail!("shell step needs `script` or `script_file`");
    };
    Ok(ctx.render(&raw)?)
}

/// Optional working directory for a step (`dir`), rendered + home-expanded.
fn step_dir(step: &Step, ctx: &Ctx) -> Result<Option<String>> {
    match step.param_str("dir") {
        Some(d) => Ok(Some(expand_home(&ctx.render(d)?)?)),
        None => Ok(None),
    }
}

#[async_trait::async_trait]
impl Processor for ShellProcessor {
    fn kind(&self) -> &str {
        "shell"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let script = resolve_script(step, ctx)?;
        run_sh(&script, &self.0, step_dir(step, ctx)?.as_deref()).await?;
        Ok(StepOutput::ok())
    }
}

// ── exec ───────────────────────────────────────────────────────────────────

pub struct ExecProcessor(Globs);

/// Build (program, args) from `program` + either `args` (array) or `argline`
/// (string, whitespace-split — mirrors codetainyrrr npm.rs split_whitespace).
fn build_exec(step: &Step, ctx: &Ctx) -> Result<(String, Vec<String>)> {
    let program = ctx.render(
        step.param_str("program")
            .context("exec step missing `program`")?,
    )?;
    let args = if let Some(arr) = step.param_array("args") {
        let mut out = Vec::new();
        for a in arr {
            let s = a.as_str().context("exec `args` entries must be strings")?;
            out.push(ctx.render(s)?);
        }
        out
    } else if let Some(line) = step.param_str("argline") {
        ctx.render(line)?
            .split_whitespace()
            .map(str::to_string)
            .collect()
    } else {
        vec![]
    };
    Ok((program, args))
}

#[async_trait::async_trait]
impl Processor for ExecProcessor {
    fn kind(&self) -> &str {
        "exec"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let (program, args) = build_exec(step, ctx)?;
        run_cmd(&program, &args, &self.0, step_dir(step, ctx)?.as_deref()).await?;
        Ok(StepOutput::ok())
    }
}

// ── merge_json (native) ─────────────────────────────────────────────────────

pub struct MergeJsonProcessor(Globs);

/// Verbatim from codetainyrrr merge_json.rs::merge_json.
fn deep_merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    if let (Some(base_obj), Some(over_obj)) = (base.as_object_mut(), overlay.as_object()) {
        for (k, v) in over_obj {
            let entry = base_obj
                .entry(k.clone())
                .or_insert(serde_json::Value::Null);
            if entry.is_object() && v.is_object() {
                deep_merge(entry, v.clone());
            } else {
                *entry = v.clone();
            }
        }
    }
}

#[async_trait::async_trait]
impl Processor for MergeJsonProcessor {
    fn kind(&self) -> &str {
        "merge_json"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let target = expand_home(&ctx.render(
            step.param_str("target").context("merge_json missing `target`")?,
        )?)?;
        let command = ctx.render(
            step.param_str("command")
                .context("merge_json missing `command`")?,
        )?;

        // Platform-aware (bash unix / powershell windows) — no hardcoded
        // bash; was a latent crash on a Windows host.
        let output =
            crate::pathenv::run_capture(&command, &self.0, step_dir(step, ctx)?.as_deref())
                .await
            .with_context(|| format!("failed to run: {command}"))?;
        if !output.status.success() {
            bail!(
                "command '{}' failed ({}): {}",
                command,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let patch: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("command stdout is not valid JSON")?;
        let mut existing: serde_json::Value = if std::path::Path::new(&target).exists() {
            let raw = std::fs::read_to_string(&target)?;
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::Object(Default::default()))
        } else {
            serde_json::Value::Object(Default::default())
        };
        deep_merge(&mut existing, patch);
        if let Some(parent) = std::path::Path::new(&target).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, serde_json::to_string_pretty(&existing)?)?;
        Ok(StepOutput::ok())
    }
}

// ── check_command ───────────────────────────────────────────────────────────

pub struct CheckCommandProcessor(Globs);

#[async_trait::async_trait]
impl Processor for CheckCommandProcessor {
    fn kind(&self) -> &str {
        "check_command"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let program = ctx.render(
            step.param_str("program")
                .context("check_command missing `program`")?,
        )?;
        if resolve_in_path(&program, &enriched_path(&self.0)).is_none() {
            let msg = step
                .param_str("on_missing")
                .map(|m| ctx.render(m))
                .transpose()?
                .unwrap_or_else(|| format!("required command '{program}' not found"));
            bail!(msg);
        }
        Ok(StepOutput::ok())
    }
}

// NOTE: there is intentionally no `claude_plugin` (or any other tool-specific)
// native processor. Claude marketplace install is the generic `marketplace`
// recipe in installer.toml — `check_command` guard + two `exec` steps. The
// engine ships only generic primitives; every tool's behavior is config.

// ── sentinel_meta ───────────────────────────────────────────────────────────

/// Spec-less meta entries: the engine marks the sentinel (B4); the step
/// itself is a no-op so deps/post_install still run.
pub struct SentinelMetaProcessor;

#[async_trait::async_trait]
impl Processor for SentinelMetaProcessor {
    fn kind(&self) -> &str {
        "sentinel_meta"
    }
    async fn run(
        &self,
        _s: &Step,
        _c: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        Ok(StepOutput::ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(src: &str) -> Step {
        Step::from_table(src.parse().unwrap()).unwrap()
    }
    fn ctx() -> Ctx {
        let mut c = Ctx::default();
        c.set("packages", "typescript eslint")
            .set("repo", "jesseduffield/lazygit")
            .set("pattern_regex", r".*linux.*\.tar\.gz");
        c
    }

    #[test]
    fn builtins_registers_generic_processors() {
        let r = builtins(&crate::config::Settings::default());
        // Only generic primitives — no tool-specific processors.
        for k in [
            "shell",
            "exec",
            "merge_json",
            "check_command",
            "sentinel_meta",
            "prompt",
            "save_input",
            "download",
            "extract",
            "copy",
            "symlink",
        ] {
            assert!(r.get(k).is_some(), "missing {k}");
        }
        assert!(r.get("claude_plugin").is_none(), "no tool-specific natives");
    }

    #[test]
    fn exec_argline_whitespace_splits_like_npm_handler() {
        let s = step(
            r#"
            type = "exec"
            program = "npm"
            argline = "install -g {{ packages }}"
            "#,
        );
        let (p, args) = build_exec(&s, &ctx()).unwrap();
        assert_eq!(p, "npm");
        assert_eq!(args, vec!["install", "-g", "typescript", "eslint"]);
    }

    #[test]
    fn exec_array_args_render_each() {
        let s = step(
            r#"
            type = "exec"
            program = "uv"
            args = ["tool", "install", "{{ packages }}"]
            "#,
        );
        let (_p, args) = build_exec(&s, &ctx()).unwrap();
        // array form does NOT whitespace-split a rendered entry
        assert_eq!(args, vec!["tool", "install", "typescript eslint"]);
    }

    #[test]
    fn shell_script_file_resolves_embedded_and_renders() {
        let s = step(
            r#"
            type = "shell"
            script_file = "recipes/gh-release.sh"
            "#,
        );
        let out = resolve_script(&s, &ctx()).unwrap();
        assert!(out.contains("jesseduffield/lazygit"));
        assert!(out.contains(r".*linux.*\.tar\.gz"));
        assert!(!out.contains("{{")); // fully rendered
    }

    #[test]
    fn shell_inline_script_renders() {
        let s = step(
            r#"
            type = "shell"
            script = "echo {{ packages }}"
            "#,
        );
        assert_eq!(resolve_script(&s, &ctx()).unwrap(), "echo typescript eslint");
    }

    #[test]
    fn shell_missing_both_errors() {
        let s = step(r#"type = "shell""#);
        assert!(resolve_script(&s, &ctx()).is_err());
    }

    #[test]
    fn deep_merge_is_verbatim_recursive() {
        let mut base = serde_json::json!({"a":{"x":1},"b":2});
        deep_merge(&mut base, serde_json::json!({"a":{"y":9},"b":3,"c":4}));
        assert_eq!(base, serde_json::json!({"a":{"x":1,"y":9},"b":3,"c":4}));
    }
}
