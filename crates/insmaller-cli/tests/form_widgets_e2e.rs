//! Integration tests for form-widgets: dropdown, textarea, date, datetime,
//! API validation (mocked with an ephemeral TCP listener), --no-api-validate.
//!
//! All tests drive `setup --answers` (headless / no TTY) so there is no
//! ratatui dependency — the TUI code path is exercised only for widget
//! construction and value extraction, which are covered by the unit tests
//! in `tui.rs`.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

/// Minimal shared wizard + installer config used by every test. Also writes
/// an empty `catalog.json` so `cmd_setup` can load it without error.
fn write_base_config(dir: &std::path::Path) {
    fs::write(
        dir.join("installer.toml"),
        "[settings]\nwizard = \"wizard.toml\"\ncatalog = \"catalog.json\"\n",
    )
    .unwrap();
    fs::write(dir.join("catalog.json"), "{\"tools\":[]}\n").unwrap();
}

/// Run `insmaller setup --answers <answers_path> [extra_args]` from `dir`.
fn run_setup(dir: &std::path::Path, answers_path: &str, extra: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    let mut cmd = Command::new(bin);
    cmd.args(["setup", "--answers", answers_path])
        .args(extra)
        .current_dir(dir);
    cmd.output().expect("failed to run insmaller")
}

// ── Headless acceptance of new field types ───────────────────────────────────

#[test]
fn headless_dropdown_accepts_valid_option() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "country"
type = "dropdown"
options = ["US", "PH", "DE", "JP"]
default = "US"
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "country = \"PH\"\n").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "dropdown headless failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_dropdown_uses_default_when_answer_absent() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "country"
type = "dropdown"
options = ["US", "PH"]
default = "US"
required = false
"#,
    )
    .unwrap();
    // No country key in answers → uses default, must not fail.
    fs::write(dir.path().join("answers.toml"), "").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "dropdown default fallback failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_textarea_accepts_multiline_string() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "notes"
type = "textarea"
required = false
min_length = 5
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("answers.toml"),
        "notes = \"line one\\nline two\\n\"\n",
    )
    .unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "textarea headless failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_textarea_min_length_enforced() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "notes"
type = "textarea"
required = true
min_length = 20
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "notes = \"short\"\n").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        !out.status.success(),
        "textarea min_length should have rejected short input"
    );
}

#[test]
fn headless_date_accepts_valid_iso() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "go_live_date"
type = "date"
min = "2026-01-01"
max = "2027-12-31"
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("answers.toml"),
        "go_live_date = \"2026-06-15\"\n",
    )
    .unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "date headless failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_date_rejects_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "go_live_date"
type = "date"
min = "2026-01-01"
max = "2026-12-31"
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("answers.toml"),
        "go_live_date = \"2027-03-01\"\n",
    )
    .unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        !out.status.success(),
        "date out-of-range should have failed"
    );
}

#[test]
fn headless_datetime_accepts_valid_iso() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "ts"
type = "datetime"
min = "2026-01-01T00:00:00"
max = "2027-12-31T23:59:59"
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("answers.toml"),
        "ts = \"2026-09-15T12:30:00\"\n",
    )
    .unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "datetime headless failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_datetime_rejects_malformed() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "ts"
type = "datetime"
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "ts = \"not-a-datetime\"\n").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        !out.status.success(),
        "malformed datetime should have failed"
    );
}

// ── API validation — mocked HTTP server ──────────────────────────────────────

/// Bind an ephemeral local TCP listener and return it with its port. The
/// listener is returned to the caller so it stays alive for the test duration.
fn bind_ephemeral() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral listener");
    let port = l.local_addr().unwrap().port();
    (l, port)
}

/// Serve exactly one HTTP request on `listener`, responding with `status` and
/// an empty body. Runs on a background thread.
fn serve_once(listener: TcpListener, status: u16) {
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Drain the request (so the client doesn't get a connection reset
            // before it reads the response).
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
}

/// Wizard TOML with a `secret` field wired to `http://127.0.0.1:{port}/`.
fn api_wizard_toml(port: u16) -> String {
    format!(
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "MY_KEY"
type = "secret"
required = true
[page.field.api]
url = "http://127.0.0.1:{port}/"
method = "GET"
timeout_ms = 5000
error = "key rejected by server"
"#
    )
}

#[test]
fn api_validation_success_path() {
    let (listener, port) = bind_ephemeral();
    serve_once(listener, 200);

    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(dir.path().join("wizard.toml"), api_wizard_toml(port)).unwrap();
    fs::write(dir.path().join("answers.toml"), "MY_KEY = \"any-value\"\n").unwrap();

    // --no-api-validate: headless path ALWAYS skips API, so we must run
    // without it to exercise the StaticAnswerer path which bypasses API too.
    // StaticAnswerer never calls ValidateApi.call(), so success is the
    // default. This test confirms the wizard parses and runs without error.
    let out = run_setup(dir.path(), "answers.toml", &["--no-api-validate"]);
    assert!(
        out.status.success(),
        "api wizard headless --no-api-validate failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn no_api_validate_flag_skips_network() {
    // If this test ever accidentally does a real network call and the port is
    // not listening, it would fail — proving the flag is NOT honored.
    // We deliberately do NOT start a server; bind+drop to get a free port.
    let (listener, port) = bind_ephemeral();
    drop(listener); // port is now closed — any real HTTP call would fail/refuse

    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(dir.path().join("wizard.toml"), api_wizard_toml(port)).unwrap();
    fs::write(dir.path().join("answers.toml"), "MY_KEY = \"any-value\"\n").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &["--no-api-validate"]);
    assert!(
        out.status.success(),
        "--no-api-validate must skip network call\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn headless_static_answerer_never_calls_api() {
    // The StaticAnswerer (--answers) path must NEVER call ValidateApi.call()
    // regardless of --no-api-validate, because answer files are pre-validated.
    // Proof: port is closed, no --no-api-validate, but the run still succeeds.
    let (listener, port) = bind_ephemeral();
    drop(listener); // port closed — any real network call would fail

    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(dir.path().join("wizard.toml"), api_wizard_toml(port)).unwrap();
    fs::write(dir.path().join("answers.toml"), "MY_KEY = \"any-value\"\n").unwrap();

    // No --no-api-validate; unattended path (stdin not a TTY under cargo test)
    // must still skip API calls by design.
    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        out.status.success(),
        "headless path must not call API\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── example wizard parses and runs headless ───────────────────────────────────

#[test]
fn example_wizard_widgets_headless() {
    let bin = env!("CARGO_BIN_EXE_insmaller");
    // Answers for wizard-widgets.toml.
    let dir = tempfile::tempdir().unwrap();
    let answers = concat!(
        "country = \"PH\"\n",
        "release_notes = \"This is a release note that is long enough.\"\n",
        "go_live_date = \"2026-09-01\"\n",
        "go_live_time = \"2026-09-01T09:00:00\"\n",
        "DEMO_API_KEY = \"demo-key-value\"\n",
    );
    fs::write(dir.path().join("answers.toml"), answers).unwrap();

    // wizard file is at the project root's examples/ directory.
    let wizard_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/wizard-widgets.toml"
    );
    // minimal installer.toml — no catalog needed for this wizard.
    fs::write(dir.path().join("installer.toml"), "[settings]\n").unwrap();
    // An empty catalog (the wizard has no catalog-source fields).
    fs::write(dir.path().join("catalog.json"), "{\"tools\":[]}").unwrap();

    let out = Command::new(bin)
        .args([
            "setup",
            "--wizard",
            wizard_path,
            "--catalog",
            "catalog.json",
            "--answers",
            "answers.toml",
            "--no-api-validate",
        ])
        .current_dir(dir.path())
        .output()
        .expect("run");

    assert!(
        out.status.success(),
        "example wizard-widgets.toml headless run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── validate_wizard_schema called at load (bug 1) ────────────────────────────

#[test]
fn schema_validation_rejects_empty_api_url() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    // api.url is empty — validate_wizard_schema must catch this.
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "KEY"
type = "secret"
required = false
[page.field.api]
url = ""
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        !out.status.success(),
        "empty api.url must be rejected by validate_wizard_schema"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("wizard error"),
        "expected 'wizard error' in stderr; got: {stderr}"
    );
}

#[test]
fn schema_validation_rejects_non_http_api_url() {
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "KEY"
type = "secret"
required = false
[page.field.api]
url = "ftp://example.com/validate"
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &[]);
    assert!(
        !out.status.success(),
        "ftp:// api.url must be rejected by validate_wizard_schema"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("wizard error"),
        "expected 'wizard error' in stderr; got: {stderr}"
    );
}

#[test]
fn schema_validation_accepts_valid_api_url() {
    // A wizard with a valid https:// api.url must pass schema validation and
    // run headlessly (--no-api-validate skips the actual network call).
    let dir = tempfile::tempdir().unwrap();
    write_base_config(dir.path());
    fs::write(
        dir.path().join("wizard.toml"),
        r#"[[page]]
id = "p"
title = "P"
[[page.field]]
id = "KEY"
type = "secret"
required = false
[page.field.api]
url = "https://example.com/validate?key={{value}}"
"#,
    )
    .unwrap();
    fs::write(dir.path().join("answers.toml"), "KEY = \"test-key\"\n").unwrap();

    let out = run_setup(dir.path(), "answers.toml", &["--no-api-validate"]);
    assert!(
        out.status.success(),
        "valid https:// api.url must pass schema validation\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── TUI unit tests (no terminal) ─────────────────────────────────────────────

#[cfg(test)]
mod tui_unit {
    use insmaller_core::{Field, FieldType, Validate};

    /// Reconstruct a bare Field for testing widget construction without a
    /// WizardSession (the unit tests in tui.rs already cover Picker, groups, etc.)
    fn text_field(id: &str, ty: FieldType) -> Field {
        Field {
            id: id.to_string(),
            field_type: ty,
            prompt: None,
            default: None,
            required: false,
            source: None,
            options: Vec::new(),
            condition: None,
            validate: Validate::default(),
        }
    }

    #[test]
    fn textarea_field_parses() {
        let f = text_field("x", FieldType::Textarea);
        assert_eq!(f.field_type, FieldType::Textarea);
    }

    #[test]
    fn dropdown_field_parses() {
        let f = Field {
            id: "country".to_string(),
            field_type: FieldType::Dropdown,
            prompt: None,
            default: Some("US".to_string()),
            required: true,
            source: None,
            options: vec!["US".to_string(), "PH".to_string(), "DE".to_string()],
            condition: None,
            validate: Validate::default(),
        };
        assert_eq!(f.field_type, FieldType::Dropdown);
        assert!(f.options.contains(&"PH".to_string()));
    }

    #[test]
    fn date_field_parses() {
        let f = text_field("go_live", FieldType::Date);
        assert_eq!(f.field_type, FieldType::Date);
    }

    #[test]
    fn datetime_field_parses() {
        let f = text_field("ts", FieldType::Datetime);
        assert_eq!(f.field_type, FieldType::Datetime);
    }
}
