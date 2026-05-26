//! PATH + home-expansion helpers. Ported from codetainyrrr handlers/mod.rs
//! verbatim, except `enriched_path` is driven by `settings.path_globs`
//! instead of a hardcoded list (the only intended generalization).

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

/// Expand `~/` and `$HOME/` prefixes to the actual home directory. Verbatim
/// from handlers/mod.rs::expand_home.
pub fn expand_home(path: &str) -> Result<String> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    if let Some(rest) = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("$HOME/"))
    {
        return Ok(home.join(rest).to_string_lossy().into_owned());
    }
    if path == "~" || path == "$HOME" {
        return Ok(home.to_string_lossy().into_owned());
    }
    Ok(path.to_owned())
}

/// Build a PATH including every dir tools install into. Re-resolved each call
/// so dirs created earlier in the same run (e.g. nvm node bin) appear without
/// a restart. Generalizes handlers/mod.rs::enriched_path: each glob entry is
/// home-expanded; `*` entries are globbed fresh, literals added as-is.
pub fn enriched_path(path_globs: &[String]) -> String {
    let home = dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/home/dev".into());

    let mut extras: Vec<String> = Vec::new();
    for entry in path_globs {
        let expanded = entry
            .strip_prefix("~/")
            .map(|r| format!("{home}/{r}"))
            .unwrap_or_else(|| entry.clone());
        if expanded.contains('*') {
            if let Ok(paths) = glob::glob(&expanded) {
                for p in paths.flatten() {
                    if p.is_dir() {
                        extras.push(p.to_string_lossy().into_owned());
                    }
                }
            }
        } else {
            extras.push(expanded);
        }
    }

    let current = std::env::var("PATH").unwrap_or_default();
    if extras.is_empty() {
        current
    } else {
        let sep = path_sep();
        format!("{}{sep}{current}", extras.join(sep))
    }
}

/// PATH separator: `:` on unix (byte-identical to the verbatim oracle), `;`
/// on Windows.
pub fn path_sep() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

/// Find `program` in colon-separated `path`. Verbatim from
/// handlers/mod.rs::resolve_in_path (needed because `Command::new` resolves
/// against the parent PATH, not the one we set on the child).
pub fn resolve_in_path(program: &str, path: &str) -> Option<PathBuf> {
    let is_explicit = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if is_explicit {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    // Windows resolves implicit names with these extensions; unix uses "" only
    // (byte-identical to the verbatim oracle).
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in path.split(path_sep()).filter(|s| !s.is_empty()) {
        for ext in exts {
            let candidate = std::path::Path::new(dir).join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Run `program args…` resolved against the enriched PATH. Verbatim from
/// handlers/mod.rs::run_cmd.
pub async fn run_cmd(
    program: &str,
    args: &[String],
    path_globs: &[String],
    dir: Option<&str>,
) -> Result<()> {
    let path = enriched_path(path_globs);
    let resolved = resolve_in_path(program, &path)
        .ok_or_else(|| anyhow::anyhow!("'{program}' not found in PATH={path}"))?;
    let mut cmd = Command::new(&resolved);
    cmd.args(args).env("PATH", &path);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let status = cmd
        .status()
        .await
        .with_context(|| format!("could not spawn '{}'", resolved.display()))?;
    if !status.success() {
        anyhow::bail!("{program} {} exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Run a shell snippet with the enriched PATH. Unix: `bash -c` (verbatim from
/// handlers/mod.rs::run_sh — the parity oracle, unchanged). Windows:
/// `powershell -NoProfile -NonInteractive -Command`, unless `prefer_bash` and a
/// `bash` is discoverable on PATH (see `shell_invocation`).
pub async fn run_sh(
    script: &str,
    path_globs: &[String],
    prefer_bash: bool,
    dir: Option<&str>,
) -> Result<()> {
    let path = enriched_path(path_globs);
    let (prog, args) = shell_invocation(script, prefer_bash, &path); // single source of dispatch
    let mut cmd = Command::new(prog);
    cmd.args(&args).env("PATH", &path);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let status = cmd
        .status()
        .await
        .with_context(|| format!("could not spawn '{prog}' (PATH={path})"))?;
    if !status.success() {
        anyhow::bail!("shell script exited with {status}");
    }
    Ok(())
}

/// `(program, args)` for running `script` in the platform shell — the single
/// place the bash↔powershell choice is made (no processor hardcodes a shell).
/// Unix is always `bash -c`. Windows is `powershell …` unless `prefer_bash` is
/// set AND a `bash` is discoverable in `path`, in which case `bash -c` (for
/// catalogs whose shell bodies are POSIX, e.g. a Git Bash dependency). `path`
/// is the same enriched PATH the caller spawns the shell with, so detection
/// and execution agree — a `bash` installed only into a `path_globs` dir is
/// still found.
pub fn shell_invocation(
    script: &str,
    prefer_bash: bool,
    path: &str,
) -> (&'static str, Vec<String>) {
    let powershell = || {
        (
            "powershell",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
        )
    };
    let bash = || ("bash", vec!["-c".into(), script.into()]);
    if cfg!(windows) {
        if prefer_bash && resolve_in_path("bash", path).is_some() {
            bash()
        } else {
            powershell()
        }
    } else {
        bash()
    }
}

/// Run a shell snippet and capture stdout (used by `merge_json`). Same
/// platform dispatch as `run_sh` — never hardcodes bash.
pub async fn run_capture(
    script: &str,
    path_globs: &[String],
    prefer_bash: bool,
    dir: Option<&str>,
) -> Result<std::process::Output> {
    let path = enriched_path(path_globs);
    let (prog, args) = shell_invocation(script, prefer_bash, &path);
    let mut cmd = Command::new(prog);
    cmd.args(&args).env("PATH", &path);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.output()
        .await
        .with_context(|| format!("could not spawn '{prog}' (PATH={path})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_handles_prefixes() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            expand_home("~/foo").unwrap(),
            home.join("foo").to_string_lossy()
        );
        assert_eq!(
            expand_home("$HOME/bar").unwrap(),
            home.join("bar").to_string_lossy()
        );
        assert_eq!(expand_home("/abs/path").unwrap(), "/abs/path");
        assert_eq!(expand_home("~").unwrap(), home.to_string_lossy());
    }

    #[test]
    fn enriched_path_prepends_literals_and_skips_unmatched_globs() {
        let p = enriched_path(&["/opt/a".into(), "/no/such/glob/*/bin".into()]);
        assert!(p.starts_with(&format!("/opt/a{}", path_sep())));
        // unmatched glob contributes nothing, not a literal with '*'
        assert!(!p.contains('*'));
    }

    #[test]
    fn enriched_path_empty_globs_is_current_path() {
        let p = enriched_path(&[]);
        assert_eq!(p, std::env::var("PATH").unwrap_or_default());
    }

    #[test]
    fn shell_invocation_dispatches_per_platform() {
        // prefer_bash = false: unchanged platform default, regardless of PATH.
        let (prog, args) = shell_invocation("echo hi", false, "");
        if cfg!(windows) {
            assert_eq!(prog, "powershell");
            assert_eq!(args.first().map(String::as_str), Some("-NoProfile"));
            assert_eq!(args.last().map(String::as_str), Some("echo hi"));
        } else {
            assert_eq!(prog, "bash");
            assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
        }
    }

    #[test]
    fn shell_invocation_prefers_bash_on_windows_using_given_path() {
        // Detection runs against the PATH passed in (the enriched spawn PATH),
        // not the process environment. Put a fake `bash` in a tempdir and feed
        // that dir as PATH.
        let dir = tempfile::tempdir().unwrap();
        let bash_name = if cfg!(windows) { "bash.exe" } else { "bash" };
        std::fs::write(dir.path().join(bash_name), b"#!/bin/sh\n").unwrap();
        let path = dir.path().to_string_lossy().into_owned();

        let (prog, _) = shell_invocation("echo hi", true, &path);
        if cfg!(windows) {
            assert_eq!(prog, "bash", "bash in the given PATH must be detected");
        } else {
            assert_eq!(prog, "bash");
        }

        // Empty PATH on Windows ⇒ no bash found ⇒ PowerShell.
        let (prog_empty, _) = shell_invocation("echo hi", true, "");
        if cfg!(windows) {
            assert_eq!(prog_empty, "powershell");
        } else {
            assert_eq!(prog_empty, "bash");
        }
    }

    #[test]
    fn resolve_in_path_explicit_path_branch() {
        // The `program.contains('/')` early return does no `:`-split, so it's
        // safe cross-platform (Windows drive-colon doesn't matter here).
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mytool");
        std::fs::write(&f, "x").unwrap();
        let fp = f.to_string_lossy().replace('\\', "/");
        assert_eq!(resolve_in_path(&fp, ""), Some(PathBuf::from(&fp)));
        assert_eq!(resolve_in_path("/no/such/abs/tool", ""), None);
    }

    // The colon-separated PATH scan is POSIX (the engine runs in a Linux
    // container, verbatim from codetainyrrr). A Windows tempdir path carries a
    // `C:` drive colon that would mis-split, so scope this to unix.
    #[cfg(unix)]
    #[test]
    fn resolve_in_path_scans_colon_separated_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mytool");
        std::fs::write(&f, "x").unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        assert_eq!(resolve_in_path("mytool", &path), Some(f));
        assert_eq!(resolve_in_path("absent", &path), None);
    }
}
