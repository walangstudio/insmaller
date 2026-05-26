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
use std::collections::{HashSet, VecDeque};

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

    run_task_body(name, cfg, reg, rep, inp, run_vars).await?;
    done.insert(name.to_string());
    Ok(())
}

/// Run a single task's own step pipeline (its `needs` are NOT run here — the
/// caller orders them). Per-OS override probed via `std::env::consts::OS`.
async fn run_task_body(
    name: &str,
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    run_vars: &Map<String, Value>,
) -> Result<()> {
    let task = cfg
        .tasks
        .get(name)
        .ok_or_else(|| EngineError::NotFound(format!("task '{name}'")))?;
    let os = std::env::consts::OS;
    let steps = task.os_steps.get(os).unwrap_or(&task.steps);

    let mut ctx = Ctx::new();
    for (k, v) in run_vars {
        ctx.set_value(k, v.clone());
    }
    ctx.set("task", name);
    rep.log(&format!("[task {name}] {} step(s)", steps.len()));
    run_step_pipeline(reg, rep, inp, steps, &ctx, name).await
}

/// Run a batch of tasks honoring `needs` ordering, with per-task concurrency.
///
/// A task with `parallel = true` (or any task when `force_parallel`) may run
/// alongside other parallel tasks whose `needs` are met, throttled by
/// `max_parallel` (`0` = unbounded). A non-`parallel` task runs **exclusively**
/// — alone, with nothing else concurrent. `needs` are pulled into the run
/// automatically and each task runs once. Fail-fast: the first error after a
/// wave aborts scheduling. The `needs` graph is cycle/unknown-checked at config
/// load, so the schedule always makes progress.
#[allow(clippy::too_many_arguments)]
pub async fn run_tasks(
    names: &[String],
    cfg: &LoadedConfig,
    reg: &ProcessorRegistry,
    rep: &dyn Reporter,
    inp: &dyn InputResolver,
    run_vars: &Map<String, Value>,
    max_parallel: usize,
    force_parallel: bool,
) -> Result<()> {
    // Transitive closure of requested tasks + their needs (BFS, deterministic).
    let mut all: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = names.iter().cloned().collect();
    while let Some(n) = queue.pop_front() {
        if !seen.insert(n.clone()) {
            continue;
        }
        let task = cfg
            .tasks
            .get(&n)
            .ok_or_else(|| EngineError::NotFound(format!("task '{n}'")))?;
        for d in &task.needs {
            queue.push_back(d.clone());
        }
        all.push(n);
    }

    let cap = if max_parallel == 0 { usize::MAX } else { max_parallel.max(1) };
    let mut done: HashSet<String> = HashSet::new();
    while done.len() < all.len() {
        let ready: Vec<&String> = all
            .iter()
            .filter(|n| {
                !done.contains(*n) && cfg.tasks[*n].needs.iter().all(|d| done.contains(d))
            })
            .collect();
        if ready.is_empty() {
            return Err(EngineError::Cycle(
                "task scheduler stalled (no runnable task)".into(),
            ));
        }
        // Parallel-eligible ready tasks run together (capped); otherwise a
        // single exclusive task runs alone this wave.
        let par: Vec<String> = ready
            .iter()
            .filter(|n| force_parallel || cfg.tasks[**n].parallel)
            .map(|n| (*n).clone())
            .take(cap)
            .collect();
        let batch = if par.is_empty() {
            vec![ready[0].clone()]
        } else {
            par
        };
        let results = futures::future::join_all(
            batch
                .iter()
                .map(|n| run_task_body(n, cfg, reg, rep, inp, run_vars)),
        )
        .await;
        for (n, r) in batch.iter().zip(results) {
            r?;
            done.insert(n.clone());
        }
    }
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

    async fn run_batch(
        names: &[&str],
        cfg: &LoadedConfig,
        max_parallel: usize,
        force_parallel: bool,
    ) -> Result<()> {
        let reg = crate::builtins(&cfg.settings);
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        run_tasks(
            &names,
            cfg,
            &reg,
            &NullReporter,
            &EnvResolver,
            &Map::new(),
            max_parallel,
            force_parallel,
        )
        .await
    }

    #[tokio::test]
    async fn run_tasks_runs_all_requested() {
        if std::env::consts::OS == "windows" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a").to_string_lossy().replace('\\', "/");
        let b = dir.path().join("b").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.a]
            parallel = true
            [[task.a.steps]]
            type = "shell"
            script = "echo a > '{a}'"
            [task.b]
            parallel = true
            [[task.b.steps]]
            type = "shell"
            script = "echo b > '{b}'"
            "#
        ));
        run_batch(&["a", "b"], &c, 0, false).await.unwrap();
        assert!(std::path::Path::new(&a).exists());
        assert!(std::path::Path::new(&b).exists());
    }

    #[tokio::test]
    async fn run_tasks_honors_needs_even_when_parallel() {
        // a needs b; even with concurrency, b must finish before a runs.
        if std::env::consts::OS == "windows" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("m").to_string_lossy().replace('\\', "/");
        let c = cfg(&format!(
            r#"
            [task.b]
            parallel = true
            [[task.b.steps]]
            type = "shell"
            script = "echo b > '{marker}'"
            [task.a]
            needs = ["b"]
            parallel = true
            [[task.a.steps]]
            type = "shell"
            script = "test -f '{marker}'"
            "#
        ));
        // request only `a`; `b` is pulled in as a need and ordered first
        run_batch(&["a"], &c, 4, false).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_tasks_actually_overlaps_in_wall_time() {
        let sleep_cmd = if std::env::consts::OS == "windows" {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let c = cfg(&format!(
            r#"
            [task.a]
            parallel = true
            [[task.a.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            [task.b]
            parallel = true
            [[task.b.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            "#
        ));
        let t = std::time::Instant::now();
        run_batch(&["a", "b"], &c, 0, false).await.unwrap();
        let elapsed = t.elapsed();
        // Two 2s sleeps run concurrently must finish well under 4s.
        assert!(
            elapsed < std::time::Duration::from_millis(3000),
            "expected overlap, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn run_tasks_single_task() {
        if std::env::consts::OS == "windows" {
            return;
        }
        let c = cfg(&format!(
            "[task.go]\n[[task.go.steps]]\n{}",
            echo_ok(std::env::consts::OS)
        ));
        run_batch(&["go"], &c, 0, false).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unflagged_tasks_run_exclusively_no_overlap() {
        // Same two 2s-sleep tasks but WITHOUT parallel=true → they must run
        // one at a time (exclusive), so wall time is ~4s, not ~2s.
        let sleep_cmd = if std::env::consts::OS == "windows" {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let c = cfg(&format!(
            r#"
            [task.a]
            [[task.a.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            [task.b]
            [[task.b.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            "#
        ));
        let t = std::time::Instant::now();
        run_batch(&["a", "b"], &c, 0, false).await.unwrap();
        assert!(
            t.elapsed() >= std::time::Duration::from_millis(3000),
            "exclusive tasks must not overlap"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn force_parallel_overrides_unflagged_tasks() {
        // No parallel flags, but force_parallel=true → they overlap (~2s).
        let sleep_cmd = if std::env::consts::OS == "windows" {
            "Start-Sleep -Seconds 2"
        } else {
            "sleep 2"
        };
        let c = cfg(&format!(
            r#"
            [task.a]
            [[task.a.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            [task.b]
            [[task.b.steps]]
            type = "shell"
            script = "{sleep_cmd}"
            "#
        ));
        let t = std::time::Instant::now();
        run_batch(&["a", "b"], &c, 0, true).await.unwrap();
        assert!(
            t.elapsed() < std::time::Duration::from_millis(3000),
            "force_parallel should overlap unflagged tasks"
        );
    }
}
