//! Dependency resolution + sentinel idempotency + pipeline execution. Ported
//! from codetainyrrr orchestrator.rs, step-based. Dep-resolve and sentinels
//! are infrastructure WRAPPING the pipeline — not steps.

use crate::config::LoadedConfig;
use crate::ctx::Ctx;
use crate::desugar::desugar;
use crate::error::{EngineError, Result};
use crate::input::InputResolver;
use crate::pathenv::{expand_home, run_sh};
use crate::registry::ProcessorRegistry;
use crate::reporter::Reporter;
use crate::sentinel::Sentinel;
use crate::step::Step;
use std::collections::HashSet;

/// One catalog entry, normalized — whatever list/shape the host stores. This
/// is the seam the host implements (the JSON `Catalog` adapter is B5).
#[derive(Debug, Clone)]
pub struct EntryRef {
    /// Sentinel namespace (codetainyrrr used "cli"/"tools"/"plugins").
    pub kind: String,
    /// Terse spec to desugar, OR `None` for inline `steps` / a meta entry.
    pub spec: Option<String>,
    /// Inline steps (bypass desugar). Mutually exclusive with a recipe spec.
    pub steps: Option<Vec<Step>>,
    pub deps: Vec<String>,
    /// Verbatim codetainyrrr behavior: bash commands run once per install.
    pub post_install: Vec<String>,
    /// Availability / install guard. Evaluated against the run's vars; false
    /// ⇒ the entry is skipped (reported, not failed).
    pub condition: Option<String>,
}

pub trait EntrySource: Send + Sync {
    fn entry(&self, key: &str) -> Option<EntryRef>;
}

#[derive(Debug, Default)]
pub struct InstallSummary {
    pub completed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// `when`: skip the step when the rendered predicate is empty / "false" / "0".
/// Ported recipes carry no `when`, so this is inert for parity.
fn when_truthy(expr: &str, ctx: &Ctx) -> Result<bool> {
    let v = ctx.render(expr)?;
    let t = v.trim();
    Ok(!(t.is_empty() || t == "false" || t == "0"))
}

fn build_ctx(
    key: &str,
    params: &serde_json::Map<String, serde_json::Value>,
    dry_run: bool,
) -> Result<Ctx> {
    let mut ctx = Ctx::new();
    ctx.set("key", key);
    ctx.set_dry_run(dry_run);
    for (k, val) in params {
        // Mirror the handlers: dest/target are home-expanded before they reach
        // the script (git_clone/merge_json expand_home()'d at runtime).
        if (k == "dest" || k == "target") && val.is_string() {
            ctx.set(k, expand_home(val.as_str().unwrap())?);
        } else {
            ctx.set_value(k, val.clone());
        }
    }
    Ok(ctx)
}

/// Run a flat step pipeline (no dep-resolution / sentinels). Public so other
/// generic drivers (e.g. named tasks) reuse the exact step semantics —
/// when/unless/requires gating, register_as, timeout, retries — without
/// reaching into the install-only `EngineCtx`.
pub async fn run_step_pipeline(
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    steps: &[Step],
    base_ctx: &Ctx,
    key: &str,
) -> Result<()> {
    run_steps(reg, rep, inp, steps, base_ctx, key, false).await
}

async fn run_steps(
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    steps: &[Step],
    base_ctx: &Ctx,
    key: &str,
    dry_run: bool,
) -> Result<()> {
    // Outputs registered by earlier steps, layered over the base ctx without
    // mutating it (Ctx is read-only by design).
    let mut registered: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

    for step in steps {
        // dry-run: report intent, never evaluate predicates, spawn, or fail
        // on missing optional input.
        if dry_run {
            rep.step_start(key, &step.kind);
            rep.log(&format!("[dry-run] {key}: would run '{}'", step.kind));
            rep.step_end(key, &step.kind, true);
            continue;
        }
        let ctx = base_ctx.with_locals(&registered);

        // `requires`: a step depending on an optional output (e.g. a skipped
        // `prompt`) is skipped, not failed — avoids strict-undefined errors.
        if !step.requires.is_empty() {
            let missing: Vec<&String> =
                step.requires.iter().filter(|v| !ctx.has(v)).collect();
            if !missing.is_empty() {
                rep.log(&format!(
                    "[{key}] {} skipped (missing required: {missing:?})",
                    step.kind
                ));
                continue;
            }
        }
        if let Some(w) = &step.when {
            if !when_truthy(w, &ctx)? {
                continue;
            }
        }
        if let Some(u) = &step.unless {
            if when_truthy(u, &ctx)? {
                continue;
            }
        }

        let proc = reg
            .get(&step.kind)
            .ok_or_else(|| EngineError::UnknownProcessor(step.kind.clone()))?;
        rep.step_start(key, &step.kind);

        // Engine-applied timeout + retries (benefits every processor).
        let mut attempt = 0u32;
        let result = loop {
            let fut = proc.run(step, &ctx, rep, inp);
            let r = match step.timeout {
                Some(secs) => match tokio::time::timeout(
                    std::time::Duration::from_secs(secs),
                    fut,
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!(
                        "step '{}' timed out after {secs}s",
                        step.kind
                    )),
                },
                None => fut.await,
            };
            if r.is_ok() || attempt >= step.retries {
                break r;
            }
            attempt += 1;
            rep.log(&format!(
                "[{key}] {} failed, retry {attempt}/{}",
                step.kind, step.retries
            ));
        };
        match result {
            Ok(out) => {
                // A deliberate skip is success, not failure — report ok and
                // note it; only a real error is `false`.
                rep.step_end(key, &step.kind, true);
                if out.skipped {
                    rep.log(&format!("[{key}] {} skipped", step.kind));
                } else {
                    for (k, v) in out.register {
                        registered.insert(k, v);
                    }
                    if let (Some(name), Some(val)) = (&step.register_as, out.value) {
                        registered.insert(name.clone(), val);
                    }
                }
            }
            Err(e) => {
                rep.step_end(key, &step.kind, false);
                if step.continue_on_error {
                    rep.log(&format!("[{key}] {} failed (continuing): {e:#}", step.kind));
                } else {
                    return Err(EngineError::StepFailed {
                        step: step.kind.clone(),
                        key: key.to_string(),
                        msg: format!("{e:#}"),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Run options. `dry_run` previews (no spawn, no sentinel, no fail on missing
/// optional input). Added as a struct so `install_many`'s signature stays
/// stable for existing callers (the strangler/codetainyrrr contract).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts {
    pub dry_run: bool,
    /// Uninstall only: override the reverse-dependency guard (remove a key
    /// even though another installed key still depends on it).
    pub force: bool,
}

/// DFS state for dependency resolution. `done` = fully processed (a diamond
/// `A→B, A→C, B→C` re-uses C harmlessly); `stack` = keys currently being
/// resolved — re-entering one is a *real* cycle, which now errors (it
/// previously short-circuited to a silent success + sentinel).
#[derive(Default)]
struct DepState {
    done: HashSet<String>,
    stack: HashSet<String>,
}

/// The shared, read-only collaborators threaded through the whole
/// install/uninstall recursion. Bundled so adding a cross-cutting concern
/// later is one field, not five signature edits. Public entry points
/// (`install_many*`, `uninstall_many*`) keep their flat signatures — that is
/// the stable codetainyrrr-integration contract.
struct EngineCtx<'a> {
    src: &'a dyn EntrySource,
    cfg: &'a LoadedConfig,
    reg: &'a ProcessorRegistry,
    rep: &'a dyn Reporter,
    inp: &'a dyn InputResolver,
    sent: &'a Sentinel,
    /// Vars an entry `condition` is evaluated against (wizard answers / env).
    /// Empty on the direct-keys path that supplies none.
    run_vars: &'a serde_json::Map<String, serde_json::Value>,
}

async fn install_with_deps(
    ec: &EngineCtx<'_>,
    key: &str,
    state: &mut DepState,
    opts: RunOpts,
) -> Result<()> {
    if state.done.contains(key) {
        return Ok(());
    }
    if !state.stack.insert(key.to_string()) {
        return Err(EngineError::Cycle(key.to_string()));
    }
    let res = Box::pin(install_body(ec, key, state, opts)).await;
    state.stack.remove(key);
    if res.is_ok() {
        state.done.insert(key.to_string());
    }
    res
}

async fn install_body(
    ec: &EngineCtx<'_>,
    key: &str,
    state: &mut DepState,
    opts: RunOpts,
) -> Result<()> {
    let (src, cfg, sent) = (ec.src, ec.cfg, ec.sent);
    let e = src
        .entry(key)
        .ok_or_else(|| EngineError::NotFound(key.to_string()))?;

    if sent.is_installed(&e.kind, key) {
        return Ok(());
    }

    // Availability guard: a conditioned-out entry is skipped (reported, not
    // failed), no deps/steps/sentinel. Holds on the direct-keys path too.
    if let Some(cond) = &e.condition {
        if !crate::wizard::eval_condition(cond, ec.run_vars) {
            ec.rep.log(&format!("[{key}] skipped (condition)"));
            return Ok(());
        }
    }

    for dep in &e.deps {
        Box::pin(install_with_deps(ec, dep, state, opts))
            .await
            .map_err(|err| {
                EngineError::DepFailed {
                    dep: dep.clone(),
                    key: key.to_string(),
                    msg: format!("{err}"),
                }
            })?;
    }

    // Resolve the step list + context.
    let mut spec_for_sentinel = "meta".to_string();
    if let Some(spec) = &e.spec {
        spec_for_sentinel = spec.clone();
        let d = desugar(spec, cfg)?;
        let recipe = cfg
            .recipe(&d.recipe)
            .ok_or_else(|| EngineError::UnknownRecipe(d.recipe.clone()))?;
        let ctx = build_ctx(key, &d.params, opts.dry_run)?;
        run_steps(ec.reg, ec.rep, ec.inp, &recipe.install, &ctx, key, opts.dry_run).await?;
        // `verify` phase: asserted success. Real state — skipped in dry-run.
        if !opts.dry_run && !recipe.verify.is_empty() {
            run_steps(ec.reg, ec.rep, ec.inp, &recipe.verify, &ctx, key, false)
                .await
                .map_err(|err| {
                    EngineError::Verify {
                        key: key.to_string(),
                        msg: format!("{err}"),
                    }
                })?;
        }
    } else if let Some(steps) = &e.steps {
        let ctx = build_ctx(key, &serde_json::Map::new(), opts.dry_run)?;
        run_steps(ec.reg, ec.rep, ec.inp, steps, &ctx, key, opts.dry_run).await?;
    } // else: pure meta entry — nothing to run, just sentinel below.

    // Dry-run previews nothing persistent: no post_install, no sentinel.
    if opts.dry_run {
        return Ok(());
    }

    // post_install: once per install, gated by the `.post` sentinel
    // (verbatim codetainyrrr semantics, run via bash with enriched PATH).
    if !e.post_install.is_empty() && !sent.post_install_done(&e.kind, key) {
        for cmd in &e.post_install {
            run_sh(cmd, &cfg.settings.path_globs, cfg.settings.prefer_bash_on_windows, None)
                .await
                .map_err(|err| {
                    EngineError::PostInstall {
                        key: key.to_string(),
                        cmd: cmd.clone(),
                        msg: format!("{err}"),
                    }
                })?;
        }
        sent.mark_post_install(&e.kind, key)?;
    }

    sent.mark(&e.kind, key, &spec_for_sentinel, None)?;
    Ok(())
}

/// Install a list of keys, resolving deps. Top-level errors are collected, not
/// propagated — siblings keep going (verbatim codetainyrrr `install_many`).
/// Signature kept stable for existing callers; use `install_many_with` for
/// dry-run.
#[allow(clippy::too_many_arguments)]
pub async fn install_many(
    src: &dyn EntrySource,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    sent: &Sentinel,
    keys: &[String],
) -> InstallSummary {
    install_many_with(src, cfg, reg, rep, inp, sent, keys, RunOpts::default(), None).await
}

/// Acquire the sentinel's cross-process lock without blocking an async worker:
/// the wait (a blocking syscall) runs on the blocking pool. `None` ⇒ locking
/// unavailable; the caller proceeds unlocked.
async fn acquire_lock(sent: &Sentinel) -> Option<crate::sentinel::LockGuard> {
    let s = sent.clone();
    tokio::task::spawn_blocking(move || s.lock())
        .await
        .ok()
        .flatten()
}

#[allow(clippy::too_many_arguments)]
pub async fn install_many_with(
    src: &dyn EntrySource,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    sent: &Sentinel,
    keys: &[String],
    opts: RunOpts,
    run_vars: Option<&serde_json::Map<String, serde_json::Value>>,
) -> InstallSummary {
    // Serialize mutating runs across processes (dry-run reads/installs nothing).
    // The wait is a blocking syscall, so acquire it on the blocking pool rather
    // than parking an async worker.
    let _lock = if opts.dry_run { None } else { acquire_lock(sent).await };
    let empty = serde_json::Map::new();
    let ec = EngineCtx {
        src,
        cfg,
        reg,
        rep,
        inp,
        sent,
        run_vars: run_vars.unwrap_or(&empty),
    };
    let mut state = DepState::default();
    let mut summary = InstallSummary::default();
    for key in keys {
        match install_with_deps(&ec, key, &mut state, opts).await {
            Ok(()) => summary.completed.push(key.clone()),
            Err(e) => summary.failed.push((key.clone(), format!("{e:#}"))),
        }
    }
    summary
}

async fn uninstall_one(ec: &EngineCtx<'_>, key: &str, opts: RunOpts) -> Result<()> {
    let (cfg, sent) = (ec.cfg, ec.sent);
    let e = ec
        .src
        .entry(key)
        .ok_or_else(|| EngineError::NotFound(key.to_string()))?;
    if !sent.is_installed(&e.kind, key) {
        return Ok(()); // not installed — uninstall is a no-op
    }
    if opts.dry_run {
        ec.rep
            .log(&format!("[dry-run] {key}: would uninstall + clear sentinel"));
        return Ok(());
    }
    if let Some(spec) = &e.spec {
        let d = desugar(spec, cfg)?;
        let recipe = cfg
            .recipe(&d.recipe)
            .ok_or_else(|| EngineError::UnknownRecipe(d.recipe.clone()))?;
        if !recipe.uninstall.is_empty() {
            let ctx = build_ctx(key, &d.params, false)?;
            run_steps(ec.reg, ec.rep, ec.inp, &recipe.uninstall, &ctx, key, false).await?;
        }
    }
    // Clear both markers so a later reinstall re-fires install + post_install
    // (verbatim codetainyrrr registry::uninstall semantics).
    sent.remove(&e.kind, key)?;
    sent.remove_post(&e.kind, key)?;
    Ok(())
}

/// Uninstall a list of keys. Non-recursive by design: removing X must not
/// remove its dependencies (others may need them) — mirrors codetainyrrr.
/// Errors are collected per key, like `install_many`.
#[allow(clippy::too_many_arguments)]
pub async fn uninstall_many(
    src: &dyn EntrySource,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    sent: &Sentinel,
    keys: &[String],
) -> InstallSummary {
    uninstall_many_with(src, cfg, reg, rep, inp, sent, keys, RunOpts::default()).await
}

#[allow(clippy::too_many_arguments)]
pub async fn uninstall_many_with(
    src: &dyn EntrySource,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    sent: &Sentinel,
    keys: &[String],
    opts: RunOpts,
) -> InstallSummary {
    let _lock = if opts.dry_run { None } else { acquire_lock(sent).await };
    let empty = serde_json::Map::new();
    let ec = EngineCtx {
        src,
        cfg,
        reg,
        rep,
        inp,
        sent,
        run_vars: &empty,
    };
    let mut summary = InstallSummary::default();
    let batch: std::collections::HashSet<&str> = keys.iter().map(String::as_str).collect();
    for key in keys {
        if !opts.force {
            let blockers = dependents_still_installed(&ec, key, &batch);
            if !blockers.is_empty() {
                summary.failed.push((
                    key.clone(),
                    format!(
                        "still required by installed: {} (use --force to override)",
                        blockers.join(", ")
                    ),
                ));
                continue;
            }
        }
        match uninstall_one(&ec, key, opts).await {
            Ok(()) => summary.completed.push(key.clone()),
            Err(e) => summary.failed.push((key.clone(), format!("{e:#}"))),
        }
    }
    summary
}

/// Installed keys (outside this uninstall batch) that directly declare `key`
/// in their `deps` — the reverse-dependency guard. Direct deps only: the
/// non-recursive uninstall never removes deps, so a transitive-only chain
/// can't exist through an uninstalled middle.
fn dependents_still_installed(
    ec: &EngineCtx<'_>,
    key: &str,
    batch: &std::collections::HashSet<&str>,
) -> Vec<String> {
    let mut deps: Vec<String> = ec
        .sent
        .installed()
        .into_iter()
        .filter(|(_, k)| k != key && !batch.contains(k.as_str()))
        .filter(|(_, k)| {
            ec.src
                .entry(k)
                .is_some_and(|e| e.deps.iter().any(|d| d == key))
        })
        .map(|(_, k)| k)
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::EnvResolver;
    use crate::processor::{Processor, StepOutput};
    use crate::reporter::NullReporter;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Records which keys' steps ran, never spawns.
    struct RecProc(Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl Processor for RecProc {
        fn kind(&self) -> &str {
            "rec"
        }
        async fn run(
            &self,
            step: &Step,
            ctx: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            let tag = step
                .param_str("tag")
                .map(|t| ctx.render(t).unwrap())
                .unwrap_or_default();
            if tag == "BOOM" {
                anyhow::bail!("intentional");
            }
            self.0.lock().unwrap().push(tag);
            Ok(StepOutput::ok())
        }
    }

    struct Src(HashMap<String, EntryRef>);
    impl EntrySource for Src {
        fn entry(&self, key: &str) -> Option<EntryRef> {
            self.0.get(key).cloned()
        }
    }

    fn step_rec(tag: &str) -> Step {
        Step::from_table(
            format!("type = \"rec\"\ntag = \"{tag}\"")
                .parse()
                .unwrap(),
        )
        .unwrap()
    }
    fn entry(deps: &[&str], tag: &str) -> EntryRef {
        EntryRef {
            kind: "tools".into(),
            spec: None,
            steps: Some(vec![step_rec(tag)]),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            post_install: vec![],
            condition: None,
        }
    }
    fn rig() -> (ProcessorRegistry, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(vec![]));
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(RecProc(log.clone())));
        (reg, log)
    }
    fn cfg() -> LoadedConfig {
        LoadedConfig::from_str("").unwrap()
    }

    // ── P0: register_as / requires / unless ────────────────────────────────
    /// Emits its rendered `emit` param as the step `value`.
    struct EmitProc;
    #[async_trait::async_trait]
    impl Processor for EmitProc {
        fn kind(&self) -> &str {
            "emit"
        }
        async fn run(
            &self,
            step: &Step,
            ctx: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            let v = ctx.render(step.param_str("emit").unwrap())?;
            Ok(StepOutput::value(v))
        }
    }

    fn step(src: &str) -> Step {
        Step::from_table(src.parse().unwrap()).unwrap()
    }
    fn steps_entry(steps: Vec<Step>) -> EntryRef {
        EntryRef {
            kind: "tools".into(),
            spec: None,
            steps: Some(steps),
            deps: vec![],
            post_install: vec![],
            condition: None,
        }
    }
    fn rig2() -> (ProcessorRegistry, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(vec![]));
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(RecProc(log.clone())));
        reg.register(Arc::new(EmitProc));
        (reg, log)
    }
    async fn run_one(reg: &ProcessorRegistry, e: EntryRef) -> InstallSummary {
        let mut m = HashMap::new();
        m.insert("x".to_string(), e);
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        install_many(
            &Src(m),
            &cfg(),
            reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await
    }

    /// Fails the first `fail_n` attempts, then succeeds.
    struct FlakyProc(Arc<Mutex<u32>>, u32);
    #[async_trait::async_trait]
    impl Processor for FlakyProc {
        fn kind(&self) -> &str {
            "flaky"
        }
        async fn run(
            &self,
            _: &Step,
            _: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            if *n <= self.1 {
                anyhow::bail!("flaky attempt {n}");
            }
            Ok(StepOutput::ok())
        }
    }

    struct SlowProc;
    #[async_trait::async_trait]
    impl Processor for SlowProc {
        fn kind(&self) -> &str {
            "slow"
        }
        async fn run(
            &self,
            _: &Step,
            _: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(StepOutput::ok())
        }
    }

    #[tokio::test]
    async fn retry_then_success_completes() {
        let count = Arc::new(Mutex::new(0u32));
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(FlakyProc(count.clone(), 1))); // fail once
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let e = steps_entry(vec![step("type=\"flaky\"\nretries=2")]);
        let mut m = HashMap::new();
        m.insert("x".to_string(), e);
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await;
        assert_eq!(s.completed, vec!["x"]);
        assert_eq!(*count.lock().unwrap(), 2); // failed once, succeeded on retry
        assert!(sent.is_installed("tools", "x"));
    }

    #[tokio::test]
    async fn step_timeout_actually_elapses() {
        let mut reg = ProcessorRegistry::new();
        reg.register(Arc::new(SlowProc));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let e = steps_entry(vec![step("type=\"slow\"\ntimeout=1")]);
        let mut m = HashMap::new();
        m.insert("x".to_string(), e);
        let src = Src(m);
        let c = cfg();
        let rep = NullReporter;
        let inp = EnvResolver;
        let keys = vec!["x".to_string()];
        let fut = install_many(&src, &c, &reg, &rep, &inp, &sent, &keys);
        let s = tokio::time::timeout(std::time::Duration::from_secs(8), fut)
            .await
            .expect("engine timeout must fire well before the 30s sleep");
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("timed out"));
        assert!(!sent.is_installed("tools", "x"));
    }

    #[tokio::test]
    async fn uninstall_unknown_key_is_an_error() {
        let (reg, _l) = rig();
        let cfg = cfg();
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = uninstall_many(
            &Src(HashMap::new()),
            &cfg,
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["ghost".into()],
        )
        .await;
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("ghost"));
    }

    #[tokio::test]
    async fn verify_success_writes_sentinel() {
        let (reg, log) = rig();
        let cfg = LoadedConfig::from_str(
            r#"
            [[desugar]]
            prefix = "v:"
            recipe = "r"
            parse  = "single_arg"
            [[recipe]]
            name = "r"
            [[recipe.install]]
            type = "rec"
            tag  = "inst"
            [[recipe.verify]]
            type = "rec"
            tag  = "vok"
            "#,
        )
        .unwrap();
        let mut m = HashMap::new();
        m.insert(
            "k".to_string(),
            EntryRef {
                kind: "tools".into(),
                spec: Some("v:x".into()),
                steps: None,
                deps: vec![],
                post_install: vec![],
                condition: None,
            },
        );
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(&Src(m), &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["k".into()])
            .await;
        assert_eq!(s.completed, vec!["k"]);
        assert_eq!(*log.lock().unwrap(), vec!["inst", "vok"]);
        assert!(sent.is_installed("tools", "k"));
    }

    #[tokio::test]
    async fn dry_run_with_post_install_runs_nothing() {
        let (reg, log) = rig();
        let mut e = entry(&[], "x");
        e.post_install = vec!["definitely-not-a-real-command-xyz".into()];
        let mut m = HashMap::new();
        m.insert("x".to_string(), e);
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many_with(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
            RunOpts { dry_run: true, ..Default::default() },
            None,
        )
        .await;
        // post_install would fail if run; dry-run must skip it entirely.
        assert_eq!(s.completed, vec!["x"]);
        assert!(s.failed.is_empty());
        assert!(log.lock().unwrap().is_empty());
        assert!(!sent.is_installed("tools", "x"));
    }

    #[tokio::test]
    async fn register_as_value_flows_to_later_step() {
        let (reg, log) = rig2();
        let e = steps_entry(vec![
            step("type=\"emit\"\nregister_as=\"v\"\nemit=\"hello-{{ key }}\""),
            step("type=\"rec\"\ntag=\"{{ v }}\""),
        ]);
        let s = run_one(&reg, e).await;
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert_eq!(*log.lock().unwrap(), vec!["hello-x"]);
    }

    #[tokio::test]
    async fn requires_missing_var_skips_step_not_errors() {
        let (reg, log) = rig2();
        let e = steps_entry(vec![
            step("type=\"rec\"\nrequires=[\"nope\"]\ntag=\"SKIPPED\""),
            step("type=\"rec\"\ntag=\"ran\""),
        ]);
        let s = run_one(&reg, e).await;
        assert_eq!(s.completed, vec!["x"]); // skip is not a failure
        assert_eq!(*log.lock().unwrap(), vec!["ran"]);
    }

    #[tokio::test]
    async fn verify_failure_fails_key_and_skips_sentinel() {
        let (reg, log) = rig();
        let cfg = LoadedConfig::from_str(
            r#"
            [[desugar]]
            prefix = "x:"
            recipe = "r"
            parse  = "single_arg"
            [[recipe]]
            name = "r"
            [[recipe.install]]
            type = "rec"
            tag  = "inst"
            [[recipe.verify]]
            type = "rec"
            tag  = "BOOM"
            "#,
        )
        .unwrap();
        let mut m = HashMap::new();
        m.insert(
            "x".to_string(),
            EntryRef {
                kind: "tools".into(),
                spec: Some("x:foo".into()),
                steps: None,
                deps: vec![],
                post_install: vec![],
                condition: None,
            },
        );
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg,
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await;
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("verify of 'x'"));
        assert_eq!(*log.lock().unwrap(), vec!["inst"]); // install ran, verify boomed
        assert!(!sent.is_installed("tools", "x")); // not marked installed
    }

    #[tokio::test]
    async fn uninstall_runs_recipe_uninstall_and_clears_sentinel() {
        let (reg, log) = rig();
        let cfg = LoadedConfig::from_str(
            r#"
            [[desugar]]
            prefix = "u:"
            recipe = "r"
            parse  = "single_arg"
            [[recipe]]
            name = "r"
            [[recipe.install]]
            type = "rec"
            tag  = "inst"
            [[recipe.uninstall]]
            type = "rec"
            tag  = "rm"
            "#,
        )
        .unwrap();
        let mut m = HashMap::new();
        m.insert(
            "k".to_string(),
            EntryRef {
                kind: "tools".into(),
                spec: Some("u:x".into()),
                steps: None,
                deps: vec![],
                post_install: vec![],
                condition: None,
            },
        );
        let src = Src(m);
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        install_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["k".into()]).await;
        assert!(sent.is_installed("tools", "k"));

        let s = uninstall_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["k".into()])
            .await;
        assert_eq!(s.completed, vec!["k"]);
        assert!(!sent.is_installed("tools", "k")); // sentinel cleared
        assert_eq!(*log.lock().unwrap(), vec!["inst", "rm"]); // uninstall steps ran

        // Uninstalling a not-installed key is a no-op success.
        let s2 =
            uninstall_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["k".into()])
                .await;
        assert_eq!(s2.completed, vec!["k"]);
        assert_eq!(log.lock().unwrap().len(), 2); // no extra steps
    }

    #[tokio::test]
    async fn uninstall_blocked_by_installed_dependent_unless_forced_or_batched() {
        let (reg, _log) = rig();
        let cfg = LoadedConfig::from_str(
            r#"
            [[desugar]]
            prefix = "u:"
            recipe = "r"
            parse  = "single_arg"
            [[recipe]]
            name = "r"
            [[recipe.install]]
            type = "rec"
            tag  = "i"
            [[recipe.uninstall]]
            type = "rec"
            tag  = "rm"
            "#,
        )
        .unwrap();
        let mk = |deps: Vec<String>| EntryRef {
            kind: "tools".into(),
            spec: Some("u:x".into()),
            steps: None,
            deps,
            post_install: vec![],
            condition: None,
        };
        let mut m = HashMap::new();
        m.insert("dep".to_string(), mk(vec![]));
        m.insert("app".to_string(), mk(vec!["dep".into()]));
        let src = Src(m);
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        install_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent,
            &["dep".into(), "app".into()]).await;
        assert!(sent.is_installed("tools", "dep") && sent.is_installed("tools", "app"));

        // 1) blocked: app still depends on dep.
        let blocked = uninstall_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent,
            &["dep".into()]).await;
        assert!(blocked.completed.is_empty());
        assert_eq!(blocked.failed.len(), 1);
        assert!(blocked.failed[0].1.contains("app"), "names the dependent");
        assert!(sent.is_installed("tools", "dep"), "not removed when blocked");

        // 2) --force overrides.
        let forced = uninstall_many_with(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent,
            &["dep".into()], RunOpts { force: true, ..Default::default() }).await;
        assert_eq!(forced.completed, vec!["dep"]);
        assert!(!sent.is_installed("tools", "dep"));

        // 3) removing the dependent in the same batch is allowed (no block).
        install_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent, &["dep".into()]).await;
        let batch = uninstall_many(&src, &cfg, &reg, &NullReporter, &EnvResolver, &sent,
            &["app".into(), "dep".into()]).await;
        assert_eq!(batch.failed, vec![]);
        assert!(!sent.is_installed("tools", "dep"));
    }

    #[tokio::test]
    async fn dry_run_spawns_nothing_and_skips_sentinel() {
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("x".into(), entry(&[], "would-run"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many_with(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
            RunOpts { dry_run: true, ..Default::default() },
            None,
        )
        .await;
        assert_eq!(s.completed, vec!["x"]);
        assert!(log.lock().unwrap().is_empty()); // processor never invoked
        assert!(!sent.is_installed("tools", "x")); // nothing persisted
    }

    #[test]
    fn json_reporter_is_a_reporter() {
        use crate::reporter::JsonReporter;
        let r = JsonReporter;
        r.step_start("k", "shell");
        r.step_end("k", "shell", true);
        r.log("hi");
    }

    #[tokio::test]
    async fn unless_truthy_skips_step() {
        let (reg, log) = rig2();
        let e = steps_entry(vec![
            step("type=\"rec\"\nunless=\"1\"\ntag=\"no\""),
            step("type=\"rec\"\nunless=\"0\"\ntag=\"yes\""),
        ]);
        let s = run_one(&reg, e).await;
        assert!(s.failed.is_empty());
        assert_eq!(*log.lock().unwrap(), vec!["yes"]);
    }

    #[tokio::test]
    async fn deps_install_before_dependents_once() {
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("ts".into(), entry(&["node"], "ts"));
        m.insert("node".into(), entry(&[], "node"));
        let src = Src(m);
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &src,
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["ts".into()],
        )
        .await;
        assert_eq!(s.completed, vec!["ts"]);
        assert!(s.failed.is_empty());
        assert_eq!(*log.lock().unwrap(), vec!["node", "ts"]);
        // idempotent: re-run does nothing (sentinel set)
        let s2 = install_many(
            &src,
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["ts".into()],
        )
        .await;
        assert_eq!(s2.completed, vec!["ts"]);
        assert_eq!(log.lock().unwrap().len(), 2); // unchanged
    }

    #[tokio::test]
    async fn dep_cycle_is_a_clear_error_not_silent_success() {
        let (reg, _l) = rig();
        let mut m = HashMap::new();
        m.insert("a".into(), entry(&["b"], "a"));
        m.insert("b".into(), entry(&["a"], "b"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["a".into()],
        )
        .await;
        // A true cycle now fails (and writes NO sentinel) instead of
        // short-circuiting to a silent success.
        assert!(s.completed.is_empty());
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("cycle"));
        assert!(!sent.is_installed("tools", "a"));
    }

    #[tokio::test]
    async fn diamond_dep_is_not_a_cycle() {
        // a→b, a→c, b→c : c is reached twice but is NOT a cycle.
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("a".into(), entry(&["b", "c"], "a"));
        m.insert("b".into(), entry(&["c"], "b"));
        m.insert("c".into(), entry(&[], "c"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["a".into()],
        )
        .await;
        assert_eq!(s.completed, vec!["a"]);
        assert!(s.failed.is_empty());
        assert_eq!(*log.lock().unwrap(), vec!["c", "b", "a"]); // c once
    }

    #[tokio::test]
    async fn failure_is_collected_siblings_continue() {
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("good".into(), entry(&[], "good"));
        m.insert("bad".into(), entry(&[], "BOOM"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["bad".into(), "good".into()],
        )
        .await;
        assert_eq!(s.completed, vec!["good"]);
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.failed[0].0, "bad");
        assert_eq!(*log.lock().unwrap(), vec!["good"]);
    }

    #[tokio::test]
    async fn missing_dependency_is_a_clear_error() {
        let (reg, _l) = rig();
        let mut m = HashMap::new();
        m.insert("x".into(), entry(&["ghost"], "x"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await;
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("ghost"));
    }

    fn cond_entry(cond: &str, tag: &str) -> EntryRef {
        let mut e = entry(&[], tag);
        e.condition = Some(cond.to_string());
        e
    }

    #[tokio::test]
    async fn conditioned_entry_skipped_when_false() {
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("x".into(), cond_entry("${OS_GATE} == 'linux'", "ran"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let mut vars = serde_json::Map::new();
        vars.insert("OS_GATE".into(), serde_json::Value::String("macos".into()));
        let s = install_many_with(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
            RunOpts::default(),
            Some(&vars),
        )
        .await;
        assert_eq!(s.completed, vec!["x"]); // skip is success, not failure
        assert!(s.failed.is_empty());
        assert!(log.lock().unwrap().is_empty()); // no steps ran
        assert!(!sent.is_installed("tools", "x")); // no sentinel
    }

    #[tokio::test]
    async fn conditioned_entry_runs_when_true() {
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("x".into(), cond_entry("${OS_GATE} == 'linux'", "ran"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let mut vars = serde_json::Map::new();
        vars.insert("OS_GATE".into(), serde_json::Value::String("linux".into()));
        let s = install_many_with(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
            RunOpts::default(),
            Some(&vars),
        )
        .await;
        assert_eq!(s.completed, vec!["x"]);
        assert_eq!(*log.lock().unwrap(), vec!["ran"]);
        assert!(sent.is_installed("tools", "x"));
    }

    #[tokio::test]
    async fn condition_skip_on_direct_install_many_path() {
        // No run_vars supplied (direct path) → ${X} empty → != 'go' is true,
        // so a guard that requires X==go skips.
        let (reg, log) = rig();
        let mut m = HashMap::new();
        m.insert("x".into(), cond_entry("${X} == 'go'", "ran"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await;
        assert_eq!(s.completed, vec!["x"]);
        assert!(log.lock().unwrap().is_empty());
        assert!(!sent.is_installed("tools", "x"));
    }

    #[tokio::test]
    async fn unknown_processor_fails_the_key() {
        let mut reg = ProcessorRegistry::new(); // no "rec" registered
        let _ = &mut reg;
        let mut m = HashMap::new();
        m.insert("x".into(), entry(&[], "x"));
        let d = tempfile::tempdir().unwrap();
        let sent = Sentinel::with_base(d.path().into());
        let s = install_many(
            &Src(m),
            &cfg(),
            &reg,
            &NullReporter,
            &EnvResolver,
            &sent,
            &["x".into()],
        )
        .await;
        assert_eq!(s.failed.len(), 1);
    }
}
