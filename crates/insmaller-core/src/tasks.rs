//! Named scriptable lifecycle tasks (`[task.*]`). A task is just an ordered,
//! per-OS, generic `Step` pipeline plus simple `needs` composition — the
//! engine knows nothing of what the steps do (Docker/systemd/k8s live
//! entirely in the script bodies in config).

use crate::config::LoadedConfig;
use crate::ctx::Ctx;
use crate::error::{EngineError, Result};
use crate::input::InputResolver;
use crate::orchestrator::run_step_pipeline;
use crate::registry::ProcessorRegistry;
use crate::reporter::Reporter;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// Run task `name`: its `needs` first (each once, cycle-guarded), then the
/// OS-selected step pipeline. `run_vars` (project.extra + env + answers) are
/// available to templating. Fails fast on the first failing step.
pub async fn run_task(
    name: &str,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    run_vars: &Map<String, Value>,
) -> Result<()> {
    let mut done = HashSet::new();
    let mut stack = Vec::new();
    run_inner(name, cfg, reg, rep, inp, run_vars, &mut done, &mut stack).await
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    name: &str,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    run_vars: &Map<String, Value>,
    done: &mut HashSet<String>,
    stack: &mut Vec<String>,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    let task = cfg
        .tasks
        .get(name)
        .ok_or_else(|| EngineError::NotFound(format!("task '{name}'")))?;
    if stack.iter().any(|s| s == name) {
        return Err(EngineError::Cycle(format!("task '{name}'")));
    }
    stack.push(name.to_string());
    for need in &task.needs {
        Box::pin(run_inner(need, cfg, reg, rep, inp, run_vars, done, stack)).await?;
    }
    stack.pop();

    // Per-OS override (probe = std::env::consts::OS), else the default steps.
    let os = std::env::consts::OS;
    let steps = task.os_steps.get(os).unwrap_or(&task.steps);

    let mut ctx = Ctx::new();
    for (k, v) in run_vars {
        ctx.set_value(k, v.clone());
    }
    ctx.set("task", name);
    rep.log(&format!("[task {name}] {} step(s)", steps.len()));
    run_step_pipeline(reg, rep, inp, steps, &ctx, name).await?;
    done.insert(name.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::EnvResolver;
    use crate::reporter::NullReporter;

    fn cfg(toml: &str) -> LoadedConfig {
        LoadedConfig::from_str(toml).unwrap()
    }
    fn vars(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }
    async fn run(name: &str, cfg: &LoadedConfig, v: &Map<String, Value>) -> Result<()> {
        let reg = crate::builtins(&cfg.settings);
        run_task(name, cfg, &reg, &NullReporter, &EnvResolver, v).await
    }

    fn echo_ok(os: &str) -> String {
        // A no-op command that exits 0 on the running platform.
        if os == "windows" {
            "type=\"shell\"\nscript=\"cmd /C exit 0\"".into()
        } else {
            "type=\"shell\"\nscript=\"true\"".into()
        }
    }

    #[tokio::test]
    async fn task_runs_steps() {
        let c = cfg(&format!(
            "[task.go]\n[[task.go.steps]]\n{}",
            echo_ok(std::env::consts::OS)
        ));
        assert!(run("go", &c, &Map::new()).await.is_ok());
    }

    #[tokio::test]
    async fn task_missing_name_is_clear_error() {
        let c = cfg("");
        let e = run("ghost", &c, &Map::new()).await.unwrap_err();
        assert!(format!("{e}").contains("task 'ghost'"));
    }

    #[tokio::test]
    async fn task_needs_runs_in_order() {
        // `b` writes a marker file; `a` needs `b` then asserts the file.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("m").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.b]
            [[task.b.steps]]
            type = "shell"
            script = "echo b > '{marker}'"
            [task.a]
            needs = ["b"]
            [[task.a.steps]]
            type = "shell"
            script = "test -f '{marker}'"
            "#
        ));
        if std::env::consts::OS != "windows" {
            assert!(run("a", &c, &Map::new()).await.is_ok());
        }
    }

    #[tokio::test]
    async fn task_nonzero_fails_fast_no_second_step() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("ran").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.x]
            [[task.x.steps]]
            type = "shell"
            script = "exit 1"
            [[task.x.steps]]
            type = "shell"
            script = "echo ran > '{m}'"
            "#
        ));
        if std::env::consts::OS != "windows" {
            assert!(run("x", &c, &Map::new()).await.is_err());
            assert!(!std::path::Path::new(&m).exists(), "second step must not run");
        }
    }

    #[tokio::test]
    async fn task_poll_until_zero_then_succeeds() {
        // `true` exits zero immediately → poll succeeds on first attempt.
        if std::env::consts::OS == "windows" {
            return;
        }
        let c = cfg(
            r#"
            [task.wait]
            [[task.wait.steps]]
            type = "shell"
            script = "true"
            poll = { attempts = 3, delay_ms = 0, until_exit_zero = true }
            "#,
        );
        assert!(run("wait", &c, &Map::new()).await.is_ok());
    }

    #[tokio::test]
    async fn task_poll_exhausts_and_fails() {
        if std::env::consts::OS == "windows" {
            return;
        }
        let c = cfg(
            r#"
            [task.wait]
            [[task.wait.steps]]
            type = "shell"
            script = "false"
            poll = { attempts = 2, delay_ms = 0, until_exit_zero = true }
            "#,
        );
        assert!(run("wait", &c, &Map::new()).await.is_err());
    }

    #[tokio::test]
    async fn task_templates_run_vars() {
        if std::env::consts::OS == "windows" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.t]
            [[task.t.steps]]
            type = "shell"
            script = "echo {{{{ PROJ_MSG }}}} > '{out}'"
            "#
        ));
        run("t", &c, &vars(&[("PROJ_MSG", "hello-proj")]))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            "hello-proj"
        );
    }

    #[tokio::test]
    async fn task_os_branch_falls_back_to_default_for_wrong_os() {
        // Only a wrong-OS override is defined → engine falls back to `steps`.
        if std::env::consts::OS == "windows" {
            return;
        }
        let wrong = if std::env::consts::OS == "linux" {
            "macos"
        } else {
            "linux"
        };
        let dir = tempfile::tempdir().unwrap();
        let o = dir.path().join("d").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.r]
            [[task.r.steps]]
            type = "shell"
            script = "echo default > '{o}'"
            [[task.r.os.{wrong}]]
            type = "shell"
            script = "echo wrong > '{o}'"
            "#
        ));
        run("r", &c, &Map::new()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&o).unwrap().trim(), "default");
    }
}
