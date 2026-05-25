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
