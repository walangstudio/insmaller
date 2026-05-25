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

/// Shared shell environment for the built-in processors: the PATH globs plus
/// the Windows bash-preference toggle. Bundled so a single `Arc` threads both
/// from `settings` to every processor that spawns a shell.
#[derive(Clone)]
struct ShellEnv {
    globs: Vec<String>,
    prefer_bash: bool,
}
type Globs = Arc<ShellEnv>;

/// Optional generic wait-loop on a step. Distinct from `Step.retries` (an
/// on-error retry the engine applies): `poll` re-runs the processor's own
/// operation up to `attempts` times with `delay_ms` between tries until it
/// exits zero. Pure config ⇒ a `wait-ready` task needs no engine knowledge of
/// what it's waiting for.
struct PollCfg {
    attempts: u32,
    delay_ms: u64,
    until_exit_zero: bool,
}

fn poll_cfg(step: &Step) -> Option<PollCfg> {
    let o = step.params.get("poll")?.as_object()?;
    Some(PollCfg {
        attempts: o.get("attempts").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
        delay_ms: o.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        until_exit_zero: o
            .get("until_exit_zero")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Run `op` once, or (when `poll.until_exit_zero`) poll it until success or
/// `attempts` is exhausted.
async fn with_poll<F, Fut>(step: &Step, mut op: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    match poll_cfg(step) {
        Some(p) if p.until_exit_zero => {
            let attempts = p.attempts.max(1);
            let mut last: Option<anyhow::Error> = None;
            for i in 0..attempts {
                match op().await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        last = Some(e);
                        if i + 1 < attempts {
                            tokio::time::sleep(std::time::Duration::from_millis(p.delay_ms))
                                .await;
                        }
                    }
                }
            }
            Err(last.unwrap_or_else(|| anyhow::anyhow!("poll exhausted")))
        }
        _ => op().await,
    }
}

/// Register all built-ins from engine `[settings]` (path globs + the
/// download hardening knobs).
pub fn builtins(settings: &crate::config::Settings) -> ProcessorRegistry {
    let g: Globs = Arc::new(ShellEnv {
        globs: settings.path_globs.clone(),
        prefer_bash: settings.prefer_bash_on_windows,
    });
    let mut r = ProcessorRegistry::new();
    r.register(Arc::new(ShellProcessor(g.clone())));
    r.register(Arc::new(ExecProcessor(g.clone())));
    r.register(Arc::new(MergeJsonProcessor(g.clone())));
    r.register(Arc::new(MergeCfgProcessor(g.clone(), CfgFmt::Toml)));
    r.register(Arc::new(MergeCfgProcessor(g.clone(), CfgFmt::Yaml)));
    r.register(Arc::new(CheckCommandProcessor(g.clone())));
    r.register(Arc::new(SentinelMetaProcessor));
    crate::processors_io::register(&mut r, settings);
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
        let dir = step_dir(step, ctx)?;
        with_poll(step, || {
            run_sh(&script, &self.0.globs, self.0.prefer_bash, dir.as_deref())
        })
        .await?;
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
        let dir = step_dir(step, ctx)?;
        with_poll(step, || {
            run_cmd(&program, &args, &self.0.globs, dir.as_deref())
        })
        .await?;
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
            crate::pathenv::run_capture(
                &command,
                &self.0.globs,
                self.0.prefer_bash,
                step_dir(step, ctx)?.as_deref(),
            )
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

// ── merge_toml / merge_yaml ─────────────────────────────────────────────────
//
// merge_json's contract for TOML/YAML targets. Kept separate from
// `MergeJsonProcessor` so its verbatim-port tests don't move; unlike it, an
// unparseable existing target is an error (never silently overwritten).

#[derive(Clone, Copy)]
enum CfgFmt {
    Toml,
    Yaml,
}

impl CfgFmt {
    fn kind(self) -> &'static str {
        match self {
            CfgFmt::Toml => "merge_toml",
            CfgFmt::Yaml => "merge_yaml",
        }
    }
    fn parse(self, raw: &str) -> Result<serde_json::Value> {
        match self {
            CfgFmt::Toml => toml::from_str(raw).context("existing target is not valid TOML"),
            CfgFmt::Yaml => serde_yaml::from_str(raw).context("existing target is not valid YAML"),
        }
    }
    fn dump(self, v: &serde_json::Value) -> Result<String> {
        match self {
            CfgFmt::Toml => toml::to_string_pretty(v)
                .context("merged document is not representable as TOML (e.g. null/non-table root)"),
            CfgFmt::Yaml => serde_yaml::to_string(v).context("serializing merged YAML"),
        }
    }
}

async fn run_merge(
    p: &MergeCfgProcessor,
    step: &Step,
    ctx: &Ctx,
) -> Result<StepOutput> {
    let fmt = p.1;
    let target = expand_home(&ctx.render(
        step.param_str("target")
            .with_context(|| format!("{} missing `target`", fmt.kind()))?,
    )?)?;
    let command = ctx.render(
        step.param_str("command")
            .with_context(|| format!("{} missing `command`", fmt.kind()))?,
    )?;
    if ctx.dry_run() {
        return Ok(StepOutput::ok());
    }
    let output = crate::pathenv::run_capture(
        &command,
        &p.0.globs,
        p.0.prefer_bash,
        step_dir(step, ctx)?.as_deref(),
    )
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
    let patch: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("command stdout is not valid JSON")?;
    let mut existing: serde_json::Value = match std::fs::read_to_string(&target) {
        Ok(raw) => fmt.parse(&raw)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(Default::default())
        }
        Err(e) => return Err(e.into()),
    };
    deep_merge(&mut existing, patch);
    crate::processors_io::atomic_write(
        std::path::Path::new(&target),
        fmt.dump(&existing)?.as_bytes(),
        None,
    )
    .with_context(|| format!("{} {target}", fmt.kind()))?;
    Ok(StepOutput::ok())
}

pub struct MergeCfgProcessor(Globs, CfgFmt);

#[async_trait::async_trait]
impl Processor for MergeCfgProcessor {
    fn kind(&self) -> &str {
        self.1.kind()
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        run_merge(self, step, ctx).await
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
        let msg = step
            .param_str("on_missing")
            .map(|m| ctx.render(m))
            .transpose()?
            .unwrap_or_else(|| format!("required command '{program}' not found"));
        // `poll` lets this wait for a binary to appear on PATH (generic
        // wait-ready); without it, a single presence check as before.
        with_poll(step, || async {
            if resolve_in_path(&program, &enriched_path(&self.0.globs)).is_some() {
                Ok(())
            } else {
                Err(anyhow::anyhow!(msg.clone()))
            }
        })
        .await?;
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
            "merge_toml",
            "merge_yaml",
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

    #[test]
    fn merge_toml_creates_and_deep_merges() {
        let mut base = CfgFmt::Toml
            .parse("[tool]\nname = \"old\"\nkeep = 1\n")
            .unwrap();
        deep_merge(
            &mut base,
            serde_json::json!({"tool":{"name":"new"},"added":true}),
        );
        let out = CfgFmt::Toml.dump(&base).unwrap();
        let back = CfgFmt::Toml.parse(&out).unwrap();
        assert_eq!(
            back,
            serde_json::json!({"tool":{"name":"new","keep":1},"added":true})
        );
    }

    #[test]
    fn merge_toml_missing_target_starts_empty() {
        let mut base = serde_json::Value::Object(Default::default());
        deep_merge(&mut base, serde_json::json!({"a":{"b":1}}));
        let back = CfgFmt::Toml.parse(&CfgFmt::Toml.dump(&base).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({"a":{"b":1}}));
    }

    #[test]
    fn merge_yaml_deep_merges() {
        let mut base = CfgFmt::Yaml.parse("provider:\n  model: a\n  keep: 1\n").unwrap();
        deep_merge(&mut base, serde_json::json!({"provider":{"model":"b"}}));
        let back = CfgFmt::Yaml.parse(&CfgFmt::Yaml.dump(&base).unwrap()).unwrap();
        assert_eq!(back, serde_json::json!({"provider":{"model":"b","keep":1}}));
    }

    #[test]
    fn merge_toml_non_table_root_errors_clearly() {
        let e = CfgFmt::Toml
            .dump(&serde_json::Value::Null)
            .unwrap_err()
            .to_string();
        assert!(e.contains("TOML"), "{e}");
    }

    #[tokio::test]
    async fn merge_toml_via_command_writes_and_dry_run_noop() {
        if std::env::consts::OS == "windows" {
            return; // command body is POSIX
        }
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("config.toml");
        std::fs::write(&target, "[a]\nkeep = 1\n").unwrap();
        let p = MergeCfgProcessor(
            Arc::new(ShellEnv { globs: vec![], prefer_bash: false }),
            CfgFmt::Toml,
        );
        let mk = |dry: bool| {
            let s = step(&format!(
                "type = \"merge_toml\"\ntarget = \"{}\"\ncommand = \"printf '{{\\\"a\\\":{{\\\"added\\\":true}}}}'\"\n",
                target.to_string_lossy()
            ));
            let mut c = Ctx::default();
            c.set_dry_run(dry);
            (s, c)
        };
        let (s, c) = mk(true);
        run_merge(&p, &s, &c).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[a]\nkeep = 1\n");
        let (s, c) = mk(false);
        run_merge(&p, &s, &c).await.unwrap();
        let back = CfgFmt::Toml
            .parse(&std::fs::read_to_string(&target).unwrap())
            .unwrap();
        assert_eq!(back, serde_json::json!({"a":{"keep":1,"added":true}}));
    }
}
