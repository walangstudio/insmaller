//! S1+S2 end-to-end: a freshly-extracted bundle installs itself.
//!
//! Drives the real `insmaller` binary (copied into a bundle dir as a shipped
//! rebrand) with a sibling `installer.toml` whose `[task.install]` copies the
//! running binary (`{{ self_exe }}`) and a sibling payload (`{{ exe_dir }}/…`).
//! Run from an unrelated cwd with no `--config`, it must still find the recipe
//! (S1: exe-sibling discovery) and resolve its own location (S2: task vars).

use std::fs;
use std::process::Command;

fn shipped_name() -> &'static str {
    if cfg!(windows) { "codetainyrrr.exe" } else { "codetainyrrr" }
}

#[test]
fn self_install_from_unrelated_cwd_with_no_config() {
    let built = env!("CARGO_BIN_EXE_insmaller");

    let bundle = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    // Ship the binary under a rebranded name next to its recipe.
    let shipped = bundle.path().join(shipped_name());
    fs::copy(built, &shipped).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&shipped).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&shipped, perm).unwrap();
    }

    // Sibling payload the recipe references via {{ exe_dir }}.
    fs::create_dir(bundle.path().join("payload")).unwrap();
    fs::write(bundle.path().join("payload/x"), b"payload-contents").unwrap();

    // Sibling recipe. dest templated from the DEST env var (cmd_task merges
    // process env into run_vars); self_exe/exe_dir come from S2.
    let recipe = r#"[task.install]
[[task.install.steps]]
type = "copy"
src  = "{{ self_exe }}"
dest = "{{ DEST }}/installed-bin"
[[task.install.steps]]
type = "copy"
src  = "{{ exe_dir }}/payload/x"
dest = "{{ DEST }}/x"
"#;
    fs::write(bundle.path().join("installer.toml"), recipe).unwrap();

    let out = Command::new(&shipped)
        .args(["task", "install"])
        .current_dir(cwd.path())
        .env("DEST", dest.path())
        .output()
        .expect("run shipped binary");

    assert!(
        out.status.success(),
        "task install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // S2: self_exe resolved to the running binary — bytes must match the
    // shipped copy, not some other file the recipe could have grabbed.
    let installed = fs::read(dest.path().join("installed-bin")).expect("installed-bin missing");
    assert_eq!(
        installed,
        fs::read(&shipped).unwrap(),
        "self_exe did not copy the running binary",
    );
    // S2: exe_dir resolved the sibling payload regardless of cwd.
    assert_eq!(
        fs::read_to_string(dest.path().join("x")).unwrap(),
        "payload-contents",
        "exe_dir/payload/x was not copied correctly",
    );
}
