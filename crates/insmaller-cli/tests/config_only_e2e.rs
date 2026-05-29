//! `[settings] setup_writes_config_only = true` makes `setup` collect config
//! and stop — it writes `setup_output`, prints the outro, and runs ZERO host
//! install scripts. Drives the real binary with `--answers` (unattended).

use std::fs;
use std::process::Command;

#[test]
fn config_only_writes_output_and_skips_install() {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    let out_env = dir.path().join("out.env");
    // A sentinel the install step would create if it ran. Its absence proves
    // the install phase was skipped.
    let marker = dir.path().join("installed.marker");

    let catalog = format!(
        r#"{{
  "clis": [
    {{
      "key": "alpha",
      "name": "Alpha CLI",
      "category": "core",
      "default": true,
      "steps": [{{ "type": "shell", "script": "echo ran > {}" }}]
    }}
  ]
}}"#,
        marker.display().to_string().replace('\\', "/")
    );
    fs::write(dir.path().join("catalog.json"), catalog).unwrap();

    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "pick"
title = "Pick"
[[page.field]]
id = "selected_clis"
type = "multiselect"
source = "catalog.clis"
"#,
    )
    .unwrap();

    let cfg = format!(
        r#"[settings]
setup_writes_config_only = true
catalog = "catalog.json"
wizard  = "wizard.toml"

[settings.setup_output]
path = "{}"
format = "env"
"#,
        out_env.display().to_string().replace('\\', "/")
    );
    fs::write(dir.path().join("installer.toml"), &cfg).unwrap();

    fs::write(dir.path().join("answers.toml"), "selected_clis = [\"alpha\"]\n").unwrap();

    let out = Command::new(bin)
        .args(["setup", "--answers", "answers.toml"])
        .current_dir(dir.path())
        .output()
        .expect("run setup");

    assert!(
        out.status.success(),
        "setup failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out_env.exists(), "setup_output was not written");
    assert!(
        !marker.exists(),
        "install step ran despite setup_writes_config_only=true",
    );
}

#[test]
fn no_args_runs_default_command() {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("installer.toml"),
        "[settings]\ndefault_command = \"status\"\n",
    )
    .unwrap();

    // No args → dispatch to `status` (always succeeds) instead of usage+fail.
    let out = Command::new(bin).current_dir(dir.path()).output().expect("run");
    assert!(
        out.status.success(),
        "no-arg default_command=status should exit 0\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // Without default_command, no-args must still fail with usage.
    let dir2 = tempfile::tempdir().unwrap();
    fs::write(dir2.path().join("installer.toml"), "[settings]\n").unwrap();
    let out2 = Command::new(bin).current_dir(dir2.path()).output().expect("run");
    assert!(!out2.status.success(), "no default_command → usage + failure");
}

#[test]
fn default_command_with_args_routes_to_default() {
    // `default_command = "status"` + `--json` → the flag must reach cmd_status
    // (proving the unknown-arg path went through the configured default, not
    // the install catch-all).
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("installer.toml"),
        "[settings]\ndefault_command = \"status\"\n",
    )
    .unwrap();
    let out = Command::new(bin)
        .args(["--json"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "default_command should absorb --json\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // cmd_status --json emits a JSON array (possibly empty).
    assert!(
        stdout.trim_start().starts_with('['),
        "expected JSON array from status --json, got: {stdout}",
    );
}

#[test]
fn default_args_are_prepended_to_user_args() {
    // `default_command = "task"` + `default_args = ["help-task"]`:
    // bare invocation must run the named task (proves prepended args reach
    // cmd_task), and a user-supplied `--jobs 1` must still parse (proves the
    // splice keeps user flags too).
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran").display().to_string().replace('\\', "/");
    let marker2 = dir
        .path()
        .join("ran2")
        .display()
        .to_string()
        .replace('\\', "/");
    let script1 = if cfg!(windows) {
        format!("cmd /C echo ok > {marker}")
    } else {
        format!("echo ok > {marker}")
    };
    let script2 = if cfg!(windows) {
        format!("cmd /C echo ok > {marker2}")
    } else {
        format!("echo ok > {marker2}")
    };
    fs::write(
        dir.path().join("installer.toml"),
        format!(
            r#"[settings]
default_command = "task"
default_args = ["help-task"]

[task.help-task]
[[task.help-task.steps]]
type = "shell"
script = "{script1}"
"#
        ),
    )
    .unwrap();
    let out = Command::new(bin)
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "bare invocation should run the configured task\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(std::path::Path::new(&marker).exists(), "task did not run");

    // Second run: user adds `--jobs 1`; default_args is still prepended, the
    // user flag is appended, and the task picks up the same step.
    fs::remove_file(&marker).unwrap();
    fs::write(
        dir.path().join("installer.toml"),
        format!(
            r#"[settings]
default_command = "task"
default_args = ["help-task"]

[task.help-task]
[[task.help-task.steps]]
type = "shell"
script = "{script2}"
"#
        ),
    )
    .unwrap();
    let out2 = Command::new(bin)
        .args(["--jobs", "1"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out2.status.success(),
        "default + user --jobs 1 should succeed\nstderr: {}",
        String::from_utf8_lossy(&out2.stderr),
    );
    assert!(
        std::path::Path::new(&marker2).exists(),
        "task did not run with --jobs 1",
    );
}

#[test]
fn explicit_subcommand_ignores_default_args() {
    // When the user passes a recognized subcommand, default_args MUST NOT be
    // spliced in — explicit always wins. Use `status` (always succeeds, no
    // side effects); `default_args` pointing at something garbage proves it
    // wasn't appended (status would otherwise treat it as a filter and emit
    // "nothing installed" rather than failing — so we check stderr, not exit).
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("installer.toml"),
        r#"[settings]
default_command = "install"
default_args = ["should-not-appear"]
"#,
    )
    .unwrap();
    // `status` runs cleanly without any reference to the default_args entry.
    let out = Command::new(bin)
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "explicit `status` must succeed regardless of default_args\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !combined.contains("should-not-appear"),
        "explicit subcommand must not see default_args; got: {combined}",
    );
}

#[test]
fn parallel_runs_all_named_tasks() {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.done").display().to_string().replace('\\', "/");
    let b = dir.path().join("b.done").display().to_string().replace('\\', "/");
    fs::write(
        dir.path().join("installer.toml"),
        format!(
            r#"[settings]

[task.a]
[[task.a.steps]]
type = "shell"
script = "echo a > {a}"

[task.b]
[[task.b.steps]]
type = "shell"
script = "echo b > {b}"
"#
        ),
    )
    .unwrap();

    let out = Command::new(bin)
        .args(["task", "a", "b", "--parallel"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "parallel task run failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(std::path::Path::new(&a).exists(), "task a did not run");
    assert!(std::path::Path::new(&b).exists(), "task b did not run");
}

#[test]
fn jobs_zero_is_rejected() {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("installer.toml"),
        "[task.t]\n[[task.t.steps]]\ntype = \"shell\"\nscript = \"echo hi\"\n",
    )
    .unwrap();
    let out = Command::new(bin)
        .args(["task", "t", "--jobs", "0"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(!out.status.success(), "--jobs 0 must be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--jobs must be >= 1"),
        "expected a clear --jobs error",
    );
}
