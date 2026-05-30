//! P1 capability processors: `prompt`, `save_input`, `download`, `extract`.
//! Kept separate from `processors.rs` so the verbatim-port oracle stays
//! untouched. Registered via `register()` from `builtins()`.

use crate::ctx::Ctx;
use crate::input::{InputResolver, PromptSpec, ResolvedInput};
use crate::pathenv::expand_home;
use crate::processor::{Processor, StepOutput};
use crate::registry::ProcessorRegistry;
use crate::reporter::Reporter;
use crate::step::Step;
use anyhow::{bail, Context as _, Result};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

pub fn register(r: &mut ProcessorRegistry, settings: &crate::config::Settings) {
    // `input` is an alias for `prompt` — the latter is the historical kind
    // name, the former reads more naturally in task pipelines that collect a
    // value from the user mid-run. Forwarding by name (not by Arc) so any
    // later `register_external` override of `prompt` also rebinds `input`.
    r.register(Arc::new(PromptProcessor));
    r.register_alias("input", "prompt");
    r.register(Arc::new(SaveInputProcessor));
    r.register(Arc::new(DownloadProcessor {
        allowed_origins: Arc::new(settings.auth_bearer_allowed_origins.clone()),
        require_sha256_for_exec: settings.require_sha256_for_exec,
    }));
    r.register(Arc::new(ExtractProcessor));
    r.register(Arc::new(CopyProcessor));
    r.register(Arc::new(SymlinkProcessor));
    r.register(Arc::new(EnsureLineProcessor));
    r.register(Arc::new(WriteEnvProcessor));
    r.register(Arc::new(BackupProcessor));
}

/// Atomic file write: write a sibling `.tmp` then rename over `path` (same
/// filesystem ⇒ the rename is atomic). On Unix `mode` is applied before the
/// rename so the file never exists world-readable. The temp file is removed
/// on any error after creation.
pub(crate) fn atomic_write(path: &Path, content: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Per-writer temp name (pid + a process-local counter) so two insmaller
    // instances writing the same target never clobber each other's temp file
    // mid-rename.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.{}.tmp", std::process::id(), seq));
    let tmp = std::path::PathBuf::from(tmp);
    let write_then_rename = || -> Result<()> {
        std::fs::write(&tmp, content)?;
        #[cfg(unix)]
        if let Some(bits) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(bits))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    match write_then_rename() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// One scalar (String/Bool/Number) as its env string; `None` for anything
/// else (Object/Array/Null) — used to flatten array elements.
fn scalar_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `KEY=value` body from vars, keys alpha-sorted, optionally filtered by
/// `include` and prefixed with a `# header` line. Scalars (String/Bool/Number)
/// emit directly. A JSON **array** of scalars emits as a comma-joined CSV
/// (`KEY=a,b,c`) so multiselect wizard fields survive into the env file and
/// round-trip through CSV `in` conditions; non-scalar elements are skipped and
/// an empty array emits `KEY=` (the key is kept so a consumer can tell
/// "selected nothing" from "key absent"). Objects/Null are dropped. Values
/// containing whitespace, `"`, `\`, `'` or `#` are double-quoted with `\`/`"`
/// escaped (dotenv-safe); the CSV is quoted as a whole if any element forces
/// it (commas themselves never trigger quoting — the consumer splits on them).
fn render_env_body(
    vars: &serde_json::Map<String, serde_json::Value>,
    header: Option<&str>,
    include: Option<&[String]>,
) -> String {
    let needs_quote = |s: &str| s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\' || c == '\'' || c == '#');
    let mut keys: Vec<&String> = vars
        .iter()
        .filter(|(k, v)| {
            matches!(
                v,
                serde_json::Value::String(_)
                    | serde_json::Value::Bool(_)
                    | serde_json::Value::Number(_)
                    | serde_json::Value::Array(_)
            ) && include.map(|inc| inc.iter().any(|i| i == *k)).unwrap_or(true)
        })
        .map(|(k, _)| k)
        .collect();
    keys.sort();
    let mut out = String::new();
    if let Some(h) = header {
        out.push_str("# ");
        out.push_str(h);
        out.push('\n');
    }
    for k in keys {
        if let serde_json::Value::Array(a) = &vars[k] {
            // Empty / all-non-scalar array ⇒ bare `KEY=` (key kept on purpose).
            let csv = a
                .iter()
                .filter_map(scalar_str)
                .collect::<Vec<_>>()
                .join(",");
            if csv.is_empty() {
                out.push_str(&format!("{k}=\n"));
                continue;
            }
            if needs_quote(&csv) {
                let esc = csv.replace('\\', "\\\\").replace('"', "\\\"");
                out.push_str(&format!("{k}=\"{esc}\"\n"));
            } else {
                out.push_str(&format!("{k}={csv}\n"));
            }
            continue;
        }
        let raw = match scalar_str(&vars[k]) {
            Some(s) => s,
            None => continue, // unreachable given the filter, but total
        };
        if needs_quote(&raw) {
            let esc = raw.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("{k}=\"{esc}\"\n"));
        } else {
            out.push_str(&format!("{k}={raw}\n"));
        }
    }
    out
}

/// Render the resolved vars to the configured setup-output file. Used by the
/// CLI after the wizard; the `write_env` processor reuses the same core so a
/// recipe can compose it too. Absent config is a no-op (caller-checked).
pub fn write_setup_output(
    so: &crate::config::SetupOutput,
    vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let crate::config::OutputFormat::Env = so.format;
    let path = expand_home(&so.path)?;
    let body = render_env_body(vars, so.header.as_deref(), so.include.as_deref());
    atomic_write(Path::new(&path), body.as_bytes(), so.mode)
        .with_context(|| format!("write_setup_output {path}"))?;
    Ok(())
}

/// `write_env` — emit the engine's resolved scalar vars to an env file
/// (atomic). Params: `path` (required, templated + home-expanded), `header`,
/// `include` (array allowlist), `mode`. Generic and composable in any recipe.
pub struct WriteEnvProcessor;

#[async_trait::async_trait]
impl Processor for WriteEnvProcessor {
    fn kind(&self) -> &str {
        "write_env"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let path = expand_home(
            &ctx.render(step.param_str("path").context("write_env needs `path`")?)?,
        )?;
        let header = match step.param_str("header") {
            Some(h) => Some(ctx.render(h)?),
            None => None,
        };
        let include: Option<Vec<String>> = step.param_array("include").map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
        let mode = step
            .param_i64("mode")
            .and_then(|m| u32::try_from(m).ok());
        let vars = match ctx.vars_json() {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let body = render_env_body(&vars, header.as_deref(), include.as_deref());
        atomic_write(Path::new(&path), body.as_bytes(), mode)
            .with_context(|| format!("write_env {path}"))?;
        Ok(StepOutput::ok())
    }
}

/// `ensure_line` — idempotently ensure `line` is present in `file` (shell
/// rc/profile, non-JSON config). Appends only if absent; creates the file +
/// parent dirs if missing. Params: `file`, `line` (both templated; `file`
/// home-expanded).
pub struct EnsureLineProcessor;

#[async_trait::async_trait]
impl Processor for EnsureLineProcessor {
    fn kind(&self) -> &str {
        "ensure_line"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let file = expand_home(
            &ctx.render(step.param_str("file").context("ensure_line needs `file`")?)?,
        )?;
        let line = ctx.render(step.param_str("line").context("ensure_line needs `line`")?)?;
        // A newline would never match the single-line idempotency check
        // (re-appended every run) and would inject extra rc lines.
        if line.contains('\n') || line.contains('\r') {
            bail!("ensure_line: `line` must be a single line (no newline)");
        }
        let path = Path::new(&file);
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        if existing.lines().any(|l| l == line) {
            return Ok(StepOutput::skipped()); // already present — no-op
        }
        if let Some(p) = path.parent() {
            if !p.as_os_str().is_empty() {
                std::fs::create_dir_all(p)?;
            }
        }
        let mut body = existing;
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&line);
        body.push('\n');
        std::fs::write(path, body).with_context(|| format!("writing {file}"))?;
        Ok(StepOutput::ok())
    }
}

/// `backup` — timestamped copy of `path` before something else mutates it.
/// Generic composable step (placed before a `merge_*`/`ensure_line` in a
/// pipeline); no coupling to any writer. Params: `path` (required, templated,
/// home-expanded), `dir` (optional, default = `path`'s parent), `suffix`
/// (optional, default `bak`). Missing `path` ⇒ skipped (nothing to back up).
/// Dry-run ⇒ no copy.
pub struct BackupProcessor;

#[async_trait::async_trait]
impl Processor for BackupProcessor {
    fn kind(&self) -> &str {
        "backup"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let path = expand_home(
            &ctx.render(step.param_str("path").context("backup needs `path`")?)?,
        )?;
        let src = Path::new(&path);
        if !src.exists() {
            return Ok(StepOutput::skipped()); // nothing to back up
        }
        let dir = match step.param_str("dir") {
            Some(d) => std::path::PathBuf::from(expand_home(&ctx.render(d)?)?),
            None => src.parent().map(Path::to_path_buf).unwrap_or_default(),
        };
        let suffix = match step.param_str("suffix") {
            Some(s) => ctx.render(s)?,
            None => "bak".to_string(),
        };
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .context("backup `path` has no file name")?;
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let dest = dir.join(format!("{name}.{ts}.{suffix}"));
        if ctx.dry_run() {
            r.log(&format!("backup (dry-run): {path} → {}", dest.display()));
            return Ok(StepOutput::ok());
        }
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&dir)?;
        }
        std::fs::copy(src, &dest)
            .with_context(|| format!("backup {path} → {}", dest.display()))?;
        r.log(&format!("backup: {} ", dest.display()));
        Ok(StepOutput::ok())
    }
}

/// Recursively copy a file or directory. Symlinks are skipped (consistent
/// with the archive extractor's link refusal) so a planted link in a cloned
/// tree can't exfiltrate files outside `src`.
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let md = src.symlink_metadata()?;
    if md.file_type().is_symlink() {
        return Ok(()); // never follow / copy a symlink
    }
    if md.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// `copy` — register skill/agent/command files into `~/.claude/...` etc.
/// Params: `src`, `dest` (both templated + home-expanded).
pub struct CopyProcessor;

#[async_trait::async_trait]
impl Processor for CopyProcessor {
    fn kind(&self) -> &str {
        "copy"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let src = expand_home(&ctx.render(step.param_str("src").context("copy needs `src`")?)?)?;
        let dest =
            expand_home(&ctx.render(step.param_str("dest").context("copy needs `dest`")?)?)?;
        copy_recursive(Path::new(&src), Path::new(&dest))
            .with_context(|| format!("copy {src} -> {dest}"))?;
        Ok(StepOutput::ok())
    }
}

/// `symlink` — link `dest` → `src` (target). Unix: real symlink. Windows:
/// real symlink if permitted (Developer Mode), else falls back to a recursive
/// copy so registration still works without admin. Params: `src`, `dest`.
pub struct SymlinkProcessor;

#[cfg(unix)]
fn make_symlink(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dest)
}
/// Directory junction via `mklink /J` — needs no SeCreateSymbolicLink
/// privilege (unlike a symlink), so it works outside Developer Mode and
/// without admin. Junctions are directory-only.
#[cfg(windows)]
fn make_dir_junction(src: &Path, dest: &Path) -> std::io::Result<()> {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(dest)
        .arg(src)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("mklink /J failed"))
    }
}

#[cfg(windows)]
fn make_symlink(src: &Path, dest: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        // symlink → junction (no privilege needed) → recursive copy.
        std::os::windows::fs::symlink_dir(src, dest)
            .or_else(|_| make_dir_junction(src, dest))
            .or_else(|_| copy_recursive(src, dest))
    } else {
        // Files: symlink, else copy (no junction equivalent for files).
        std::os::windows::fs::symlink_file(src, dest)
            .or_else(|_| copy_recursive(src, dest))
    }
}

#[async_trait::async_trait]
impl Processor for SymlinkProcessor {
    fn kind(&self) -> &str {
        "symlink"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let src =
            expand_home(&ctx.render(step.param_str("src").context("symlink needs `src`")?)?)?;
        let dest =
            expand_home(&ctx.render(step.param_str("dest").context("symlink needs `dest`")?)?)?;
        let (sp, dp) = (Path::new(&src), Path::new(&dest));
        if let Some(p) = dp.parent() {
            std::fs::create_dir_all(p)?;
        }
        // Idempotent replace of an existing symlink/file. Refuse to
        // `remove_dir_all` a REAL (non-symlink) directory at dest — a
        // template/typo-derived `dest` must never nuke an unrelated tree.
        if let Ok(md) = dp.symlink_metadata() {
            let ft = md.file_type();
            if ft.is_dir() && !ft.is_symlink() {
                bail!(
                    "symlink: dest '{dest}' is an existing real directory — refusing destructive replace"
                );
            }
            let _ = std::fs::remove_file(dp).or_else(|_| std::fs::remove_dir_all(dp));
        }
        make_symlink(sp, dp).with_context(|| format!("symlink {dest} -> {src}"))?;
        Ok(StepOutput::ok())
    }
}


fn prompt_spec(step: &Step, ctx: &Ctx) -> Result<(String, PromptSpec)> {
    let name = step
        .param_str("name")
        .context("prompt/save_input needs `name`")?
        .to_string();
    let env_key = step.param_str("env").unwrap_or(&name).to_string();
    let message = match step.param_str("message") {
        Some(m) => ctx.render(m)?,
        None => name.clone(),
    };
    Ok((
        name,
        PromptSpec {
            env_key,
            message,
            required: step.param_bool("required").unwrap_or(true),
            secret: step.param_bool("secret").unwrap_or(false),
        },
    ))
}

// ── prompt ──────────────────────────────────────────────────────────────────

pub struct PromptProcessor;

#[async_trait::async_trait]
impl Processor for PromptProcessor {
    fn kind(&self) -> &str {
        "prompt"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        inp: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let (name, spec) = prompt_spec(step, ctx)?;
        // NOTE: the `confirm` gate is enforced generically by the orchestrator
        // against this step's output value (see `confirm_gate`), not here, so
        // every value-producing step gets it — not just `prompt`/`input`.
        match inp.resolve(&name, &spec) {
            ResolvedInput::Value(v) => {
                let mut out = StepOutput::value(v.clone());
                out.register.insert(name, serde_json::Value::String(v));
                Ok(out)
            }
            // Optional + not provided: register nothing; dependents must
            // declare `requires` so they skip rather than strict-undefined.
            // A skip produces no value, so the `confirm` gate is a no-op.
            ResolvedInput::Skip => Ok(StepOutput::skipped()),
            // Required + not provided in a non-interactive context: fail fast,
            // never block (the EnvResolver contract).
            ResolvedInput::Fail(m) => bail!(m),
        }
    }
}

// ── save_input ──────────────────────────────────────────────────────────────

pub struct SaveInputProcessor;

fn upsert_env_line(path: &Path, key: &str, value: &str) -> Result<()> {
    // A newline (or `=` in the key) would forge extra assignments when the
    // file is sourced / dotenv-parsed (e.g. overwrite PATH).
    if key.contains(['\n', '\r', '=']) {
        bail!("save_input: name/env must not contain newline or '='");
    }
    if value.contains(['\n', '\r']) {
        bail!("save_input: value must not contain a newline");
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line.split('=').next() == Some(key) && line.contains('=') {
            out.push(format!("{key}={value}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(format!("{key}={value}"));
    }
    let mut body = out.join("\n");
    body.push('\n');
    std::fs::write(path, body)?;
    Ok(())
}

#[async_trait::async_trait]
impl Processor for SaveInputProcessor {
    fn kind(&self) -> &str {
        "save_input"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        inp: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let (name, spec) = prompt_spec(step, ctx)?;
        // Explicit `value` wins; otherwise resolve like `prompt`.
        let value = match step.param_str("value") {
            Some(v) => ctx.render(v)?,
            None => match inp.resolve(&name, &spec) {
                ResolvedInput::Value(v) => v,
                ResolvedInput::Skip => return Ok(StepOutput::skipped()),
                ResolvedInput::Fail(m) => bail!(m),
            },
        };
        let file = expand_home(step.param_str("file").unwrap_or(".env"))?;
        upsert_env_line(Path::new(&file), &spec.env_key, &value)?;
        let mut out = StepOutput::value(value.clone());
        out.register.insert(name, serde_json::Value::String(value));
        Ok(out)
    }
}

// ── download ────────────────────────────────────────────────────────────────

pub struct DownloadProcessor {
    pub allowed_origins: Arc<Vec<String>>,
    pub require_sha256_for_exec: bool,
}

/// `scheme://host[:port]` of a URL, lowercased — for the bearer allowlist.
/// `None` if the authority carries userinfo (`user@host`): the connect host
/// is then decoupled from a naive prefix check, so we fail closed.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{}://{}", scheme.to_lowercase(), authority.to_lowercase()))
}

fn mode_is_executable(mode: &str) -> bool {
    u32::from_str_radix(mode.trim_start_matches("0o"), 8)
        .map(|b| b & 0o111 != 0)
        .unwrap_or(false)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[async_trait::async_trait]
impl Processor for DownloadProcessor {
    fn kind(&self) -> &str {
        "download"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let url = ctx.render(step.param_str("url").context("download needs `url`")?)?;
        let dest = expand_home(&ctx.render(
            step.param_str("dest").context("download needs `dest`")?,
        )?)?;
        let expected = step.param_str("sha256").map(str::to_string);
        // Optional bearer auth from an env var (e.g. GITHUB_TOKEN) — mirrors
        // the gh-release recipe; raises GitHub's rate limit.
        let bearer = step
            .param_str("auth_bearer_env")
            .and_then(|n| std::env::var(n).ok())
            .filter(|t| !t.is_empty());
        let mode = step.param_str("mode").map(str::to_string);

        // Hardening: an executable download must carry an integrity hash
        // when the operator opted in. "Executable" = a unix exec `mode` OR an
        // explicit `executable = true` (the latter works cross-platform,
        // since Windows ignores `mode`).
        let is_exec = step.param_bool("executable").unwrap_or(false)
            || mode.as_deref().map(mode_is_executable).unwrap_or(false);
        if self.require_sha256_for_exec && expected.is_none() && is_exec {
            bail!("download of an executable ({url}) requires `sha256` (require_sha256_for_exec)");
        }
        // Hardening: never leak a bearer token to an off-allowlist / non-TLS
        // origin (token-exfil guard).
        if bearer.is_some() {
            if !url.starts_with("https://") {
                bail!("auth_bearer_env requires an https:// url, got {url}");
            }
            // Userinfo decouples the prefix check from the real connect host
            // (`https://api.github.com@evil.com`) — refuse outright.
            if origin_of(&url).is_none() {
                bail!("auth_bearer_env: url must have a plain scheme://host (no userinfo): {url}");
            }
            if !self.allowed_origins.is_empty() {
                let ok = origin_of(&url)
                    .map(|o| self.allowed_origins.iter().any(|a| a.to_lowercase() == o))
                    .unwrap_or(false);
                if !ok {
                    bail!(
                        "auth_bearer_env not allowed for {url}: origin not in auth_bearer_allowed_origins"
                    );
                }
            }
        }
        // Self-bounding: the engine timeout can't kill spawn_blocking, so the
        // download enforces its own ceiling (step `timeout`, default 600s).
        let dl_timeout = step.timeout.unwrap_or(600);

        // ureq is blocking; keep it off the async worker.
        let (url2, dest2) = (url.clone(), dest.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(dl_timeout)))
                .build()
                .into();
            let mut req = agent.get(&url2).header("User-Agent", "insmaller");
            if let Some(tok) = bearer {
                req = req.header("Authorization", &format!("Bearer {tok}"));
            }
            let mut resp = req.call().with_context(|| format!("GET {url2}"))?;
            let code = resp.status().as_u16();
            if !(200..300).contains(&code) {
                bail!("download {url2} -> HTTP {code}");
            }
            let bytes = resp
                .body_mut()
                .read_to_vec()
                .context("reading response body")?;
            if let Some(exp) = expected {
                let got = sha256_hex(&bytes);
                if !exp.eq_ignore_ascii_case(&got) {
                    bail!("sha256 mismatch for {url2}: expected {exp}, got {got}");
                }
            }
            if let Some(parent) = Path::new(&dest2).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest2, &bytes).with_context(|| format!("writing {dest2}"))?;
            #[cfg(unix)]
            if let Some(m) = mode {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(bits) = u32::from_str_radix(m.trim_start_matches("0o"), 8) {
                    std::fs::set_permissions(
                        &dest2,
                        std::fs::Permissions::from_mode(bits),
                    )?;
                }
            }
            #[cfg(not(unix))]
            let _ = mode;
            Ok(())
        })
        .await
        .context("download task panicked")??;
        Ok(StepOutput::ok())
    }
}

// ── extract ─────────────────────────────────────────────────────────────────

pub struct ExtractProcessor;

/// Strip the first `n` path components (tar/zip `--strip-components`).
fn strip(path: &Path, n: usize) -> Option<std::path::PathBuf> {
    let mut comps = path.components();
    for _ in 0..n {
        comps.next()?;
    }
    let rest: std::path::PathBuf = comps.as_path().to_path_buf();
    if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Join `rel` under `dest`, rejecting any entry that escapes `dest` via `..`,
/// an absolute path, or a Windows drive/prefix (zip-slip / tar traversal).
fn safe_join(dest: &Path, rel: &Path) -> Result<std::path::PathBuf> {
    use std::path::Component;
    let mut out = dest.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "archive entry '{}' escapes the destination (path traversal)",
                    rel.display()
                );
            }
        }
    }
    Ok(out)
}

fn untar<R: Read>(reader: R, dest: &Path, strip_n: usize) -> Result<()> {
    let mut ar = tar::Archive::new(reader);
    for entry in ar.entries()? {
        let mut entry = entry?;
        // Symlink/hardlink entries enable write-outside-dest via a later
        // entry that resolves through them — refuse them outright.
        let et = entry.header().entry_type();
        if et.is_symlink() || et.is_hard_link() {
            bail!(
                "archive contains a link entry ('{}') — refused (traversal risk)",
                entry.path()?.display()
            );
        }
        let path = entry.path()?.into_owned();
        let Some(rel) = strip(&path, strip_n) else {
            continue;
        };
        let out = safe_join(dest, &rel)?;
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        entry.unpack(&out)?;
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest: &Path, strip_n: usize) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let name = archive.to_string_lossy().to_lowercase();
    let f = std::fs::File::open(archive)
        .with_context(|| format!("opening archive {}", archive.display()))?;
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(f)?;
        for i in 0..zip.len() {
            let mut e = zip.by_index(i)?;
            // Symmetric with the tar guard: a zip symlink entry (unix mode
            // S_IFLNK) can point outside dest and let a later entry escape.
            if e.unix_mode().map(|m| m & 0o170000 == 0o120000).unwrap_or(false) {
                bail!(
                    "archive contains a symlink entry ('{}') — refused (traversal risk)",
                    e.name()
                );
            }
            let Some(ep) = e.enclosed_name() else { continue };
            let Some(rel) = strip(&ep, strip_n) else {
                continue;
            };
            let out = safe_join(dest, &rel)?;
            if e.is_dir() {
                std::fs::create_dir_all(&out)?;
            } else {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut w = std::fs::File::create(&out)?;
                std::io::copy(&mut e, &mut w)?;
            }
        }
        Ok(())
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        untar(flate2::read::GzDecoder::new(f), dest, strip_n)
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
        untar(bzip2::read::BzDecoder::new(f), dest, strip_n)
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        untar(xz2::read::XzDecoder::new(f), dest, strip_n)
    } else if name.ends_with(".tar") {
        untar(f, dest, strip_n)
    } else if name.ends_with(".gz") {
        // single-file gzip → dest/<archive stem>
        let stem = archive.file_stem().context("gz archive has no stem")?;
        let mut dec = flate2::read::GzDecoder::new(f);
        let mut w = std::fs::File::create(dest.join(stem))?;
        std::io::copy(&mut dec, &mut w)?;
        Ok(())
    } else {
        bail!("extract: unsupported archive type for {}", archive.display())
    }
}

#[async_trait::async_trait]
impl Processor for ExtractProcessor {
    fn kind(&self) -> &str {
        "extract"
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        _r: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let archive = expand_home(&ctx.render(
            step.param_str("archive").context("extract needs `archive`")?,
        )?)?;
        let dest = expand_home(&ctx.render(
            step.param_str("dest").context("extract needs `dest`")?,
        )?)?;
        let strip_n = step.param_i64("strip_components").unwrap_or(0).max(0) as usize;
        extract_archive(Path::new(&archive), Path::new(&dest), strip_n)?;
        Ok(StepOutput::ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{EnvResolver, StaticResolver};
    use crate::reporter::NullReporter;
    use std::collections::HashMap;
    use std::io::Write;

    fn ctx() -> Ctx {
        let mut c = Ctx::default();
        c.set("key", "demo");
        c
    }
    fn step(src: &str) -> Step {
        Step::from_table(src.parse().unwrap()).unwrap()
    }
    fn p(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn prompt_value_registers_and_skip_is_not_failure() {
        let mut m = HashMap::new();
        m.insert("TOKEN".to_string(), "secret".to_string());
        let r = StaticResolver(m);
        let out = rt()
            .block_on(PromptProcessor.run(
                &step("type=\"prompt\"\nname=\"TOKEN\"\nrequired=true"),
                &ctx(),
                &NullReporter,
                &r,
            ))
            .unwrap();
        assert_eq!(
            out.register.get("TOKEN").unwrap().as_str(),
            Some("secret")
        );
        // optional + absent → skipped, not error
        let r2 = StaticResolver(HashMap::new());
        let out2 = rt()
            .block_on(PromptProcessor.run(
                &step("type=\"prompt\"\nname=\"OPT\"\nrequired=false"),
                &ctx(),
                &NullReporter,
                &r2,
            ))
            .unwrap();
        assert!(out2.skipped);
    }

    #[test]
    fn prompt_required_missing_fails_fast() {
        let r = StaticResolver(HashMap::new());
        let res = rt().block_on(PromptProcessor.run(
            &step("type=\"prompt\"\nname=\"NEED\"\nrequired=true"),
            &ctx(),
            &NullReporter,
            &r,
        ));
        assert!(res.is_err());
    }

    #[test]
    fn prompt_value_path_returns_resolved_value() {
        // `confirm` is now enforced by the orchestrator (see orchestrator
        // tests); PromptProcessor itself just resolves + registers the value.
        let mut m = HashMap::new();
        m.insert("CONFIRM".to_string(), "RESET".to_string());
        let r = StaticResolver(m);
        let out = rt()
            .block_on(PromptProcessor.run(
                &step("type=\"prompt\"\nname=\"CONFIRM\"\nrequired=true\nconfirm=\"RESET\""),
                &ctx(),
                &NullReporter,
                &r,
            ))
            .unwrap();
        assert_eq!(out.register.get("CONFIRM").unwrap().as_str(), Some("RESET"));
        assert_eq!(out.value.unwrap().as_str(), Some("RESET"));
    }

    #[test]
    fn input_alias_reaches_prompt_processor() {
        // `register()` binds prompt under "prompt" and forwards "input" → "prompt";
        // a recipe written `type = "input"` resolves to the same processor.
        let mut reg = ProcessorRegistry::new();
        register(&mut reg, &crate::config::Settings::default());
        let prompt = reg.get("prompt").expect("prompt missing");
        let input = reg.get("input").expect("input alias missing");
        assert!(Arc::ptr_eq(&prompt, &input));
    }

    #[test]
    fn prompt_optional_absent_skips() {
        // Optional + absent → Skip (no value), so the orchestrator's `confirm`
        // gate is a no-op even if `confirm` referenced an undefined var.
        let r = StaticResolver(HashMap::new());
        let out = rt()
            .block_on(PromptProcessor.run(
                &step(
                    "type=\"prompt\"\nname=\"OPT\"\nrequired=false\nconfirm=\"{{ definitely_not_in_ctx }}\"",
                ),
                &ctx(),
                &NullReporter,
                &r,
            ))
            .unwrap();
        assert!(out.skipped);
    }

    #[test]
    fn save_input_explicit_value_upserts_file_and_registers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env.txt");
        std::fs::write(&f, "KEEP=1\nNAME=old\n").unwrap();
        let src = format!(
            "type=\"save_input\"\nname=\"NAME\"\nvalue=\"new-{{{{ key }}}}\"\nfile=\"{}\"",
            f.to_string_lossy().replace('\\', "/")
        );
        let out = rt()
            .block_on(SaveInputProcessor.run(
                &step(&src),
                &ctx(),
                &NullReporter,
                &StaticResolver(HashMap::new()),
            ))
            .unwrap();
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.contains("KEEP=1"));
        assert!(body.contains("NAME=new-demo"));
        assert!(!body.contains("NAME=old"));
        assert_eq!(out.register.get("NAME").unwrap().as_str(), Some("new-demo"));
    }

    #[test]
    fn extract_targz_and_zip_with_strip_components() {
        let dir = tempfile::tempdir().unwrap();
        // build top/bin/tool inside a .tar.gz
        let tgz = dir.path().join("a.tar.gz");
        {
            let f = std::fs::File::create(&tgz).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut tb = tar::Builder::new(enc);
            let data = b"#!bin\n";
            let mut hdr = tar::Header::new_gnu();
            hdr.set_size(data.len() as u64);
            hdr.set_mode(0o755);
            tb.append_data(&mut hdr, "top/bin/tool", &data[..]).unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("o1");
        extract_archive(&tgz, &out, 1).unwrap();
        assert!(out.join("bin/tool").is_file(), "strip 1 → bin/tool");

        // zip with one entry deep/x.txt, strip 1 → x.txt
        let zp = dir.path().join("b.zip");
        {
            let f = std::fs::File::create(&zp).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            z.start_file("deep/x.txt", opts).unwrap();
            z.write_all(b"hi").unwrap();
            z.finish().unwrap();
        }
        let out2 = dir.path().join("o2");
        extract_archive(&zp, &out2, 1).unwrap();
        assert_eq!(std::fs::read_to_string(out2.join("x.txt")).unwrap(), "hi");
    }

    #[tokio::test]
    async fn copy_processor_copies_file_and_dir_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        std::fs::write(dir.path().join("src/a.txt"), "A").unwrap();
        std::fs::write(dir.path().join("src/sub/b.txt"), "B").unwrap();
        let s = format!(
            "type=\"copy\"\nsrc=\"{}\"\ndest=\"{}\"",
            p(&dir.path().join("src")),
            p(&dir.path().join("dst"))
        );
        CopyProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst/a.txt")).unwrap(),
            "A"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst/sub/b.txt")).unwrap(),
            "B"
        );
    }

    #[tokio::test]
    async fn symlink_processor_makes_dest_resolve_to_src_content() {
        // Cross-platform: unix → real symlink; windows → link or copy
        // fallback. Either way dest must yield src's content + be idempotent.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "hi").unwrap();
        let s = format!(
            "type=\"symlink\"\nsrc=\"{}\"\ndest=\"{}\"",
            p(&dir.path().join("real.txt")),
            p(&dir.path().join("link.txt"))
        );
        for _ in 0..2 {
            SymlinkProcessor
                .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
                .await
                .unwrap(); // second run = idempotent replace
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("link.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn ensure_line_is_idempotent_and_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("sub/.bashrc");
        let s = format!(
            "type=\"ensure_line\"\nfile=\"{}\"\nline=\"export FOO=1\"",
            p(&rc)
        );
        // 1st: creates parent + file + appends
        let o1 = EnsureLineProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert!(!o1.skipped);
        // 2nd: already present → skipped, no duplicate
        let o2 = EnsureLineProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert!(o2.skipped);
        let body = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(body.matches("export FOO=1").count(), 1);
    }

    #[tokio::test]
    async fn ensure_line_rejects_embedded_newline() {
        let dir = tempfile::tempdir().unwrap();
        let s = format!(
            "type=\"ensure_line\"\nfile=\"{}\"\nline=\"a\\nexport EVIL=1\"",
            p(&dir.path().join("rc"))
        );
        let r = EnsureLineProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await;
        assert!(format!("{:#}", r.unwrap_err()).contains("single line"));
    }

    #[tokio::test]
    async fn save_input_rejects_newline_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let s = format!(
            "type=\"save_input\"\nname=\"K\"\nvalue=\"ok\\nPATH=/evil\"\nfile=\"{}\"",
            p(&dir.path().join("e"))
        );
        let r = SaveInputProcessor
            .run(&step(&s), &ctx(), &NullReporter, &StaticResolver(HashMap::new()))
            .await;
        assert!(format!("{:#}", r.unwrap_err()).contains("newline"));
    }

    #[tokio::test]
    async fn symlink_refuses_to_destroy_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), "x").unwrap();
        let victim = dir.path().join("important");
        std::fs::create_dir_all(victim.join("keep")).unwrap();
        let s = format!(
            "type=\"symlink\"\nsrc=\"{}\"\ndest=\"{}\"",
            p(&dir.path().join("real.txt")),
            p(&victim)
        );
        let r = SymlinkProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await;
        assert!(format!("{:#}", r.unwrap_err()).contains("real directory"));
        assert!(victim.join("keep").is_dir(), "victim dir untouched");
    }

    #[test]
    fn origin_and_mode_helpers() {
        assert_eq!(
            origin_of("https://API.github.com/x/y?a=1").as_deref(),
            Some("https://api.github.com")
        );
        assert_eq!(origin_of("not-a-url"), None);
        // userinfo decouples the connect host from a prefix check: fail closed.
        assert_eq!(origin_of("https://evil@github.com/x"), None);
        assert_eq!(origin_of("https://github.com@evil.test/x"), None);
        assert!(mode_is_executable("0o755"));
        assert!(mode_is_executable("755"));
        assert!(!mode_is_executable("0o644"));
    }

    fn dl(allowed: &[&str], require_sha: bool) -> DownloadProcessor {
        DownloadProcessor {
            allowed_origins: Arc::new(allowed.iter().map(|s| s.to_string()).collect()),
            require_sha256_for_exec: require_sha,
        }
    }

    #[test]
    fn bearer_requires_https_and_allowlist_before_any_network() {
        std::env::set_var("INSM_TESTTOK", "secret");
        // http:// with a bearer → refused before any request.
        let e = rt().block_on(dl(&[], false).run(
            &step("type=\"download\"\nurl=\"http://x/y\"\ndest=\"/tmp/x\"\nauth_bearer_env=\"INSM_TESTTOK\""),
            &ctx(),
            &NullReporter,
            &EnvResolver,
        ));
        assert!(format!("{:#}", e.unwrap_err()).contains("https"));
        // https but origin not in a non-empty allowlist → refused.
        let e2 = rt().block_on(dl(&["https://api.github.com"], false).run(
            &step("type=\"download\"\nurl=\"https://evil.example/x\"\ndest=\"/tmp/x\"\nauth_bearer_env=\"INSM_TESTTOK\""),
            &ctx(),
            &NullReporter,
            &EnvResolver,
        ));
        assert!(format!("{:#}", e2.unwrap_err()).contains("not in"));
        std::env::remove_var("INSM_TESTTOK");
    }

    #[test]
    fn require_sha256_for_exec_blocks_unhashed_executable() {
        let e = rt().block_on(dl(&[], true).run(
            &step("type=\"download\"\nurl=\"https://x/y\"\ndest=\"/tmp/t\"\nmode=\"0o755\""),
            &ctx(),
            &NullReporter,
            &EnvResolver,
        ));
        assert!(format!("{:#}", e.unwrap_err()).contains("requires `sha256`"));
    }

    #[test]
    fn extract_unsupported_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rar");
        std::fs::write(&f, b"x").unwrap();
        let err = extract_archive(&f, &dir.path().join("o"), 0).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported archive"));
    }

    #[test]
    fn safe_join_blocks_escape() {
        let d = Path::new("/dest");
        assert!(safe_join(d, Path::new("a/b")).unwrap().ends_with("a/b"));
        assert!(safe_join(d, Path::new("./a")).unwrap().ends_with("a"));
        assert!(safe_join(d, Path::new("../escape")).is_err());
        assert!(safe_join(d, Path::new("a/../../escape")).is_err());
        assert!(safe_join(d, Path::new("/abs/evil")).is_err());
    }

    #[test]
    fn extract_refuses_symlink_entry() {
        let dir = tempfile::tempdir().unwrap();
        let tar = dir.path().join("link.tar");
        {
            let f = std::fs::File::create(&tar).unwrap();
            let mut tb = tar::Builder::new(f);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            tb.append_link(&mut h, "evil", "/etc/passwd").unwrap();
            tb.into_inner().unwrap();
        }
        let err = extract_archive(&tar, &dir.path().join("o"), 0).unwrap_err();
        assert!(format!("{err:#}").contains("link entry"));
    }

    #[test]
    fn extract_total_strip_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("a.tar.gz");
        {
            let f = std::fs::File::create(&tgz).unwrap();
            let enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut tb = tar::Builder::new(enc);
            let data = b"x";
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            tb.append_data(&mut h, "top/x", &data[..]).unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("o");
        extract_archive(&tgz, &out, 5).unwrap(); // strip more than depth
        assert!(std::fs::read_dir(&out).unwrap().next().is_none());
    }

    fn jmap(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn write_env_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("out.env");
        let so = crate::config::SetupOutput {
            path: f.to_string_lossy().into_owned(),
            format: crate::config::OutputFormat::Env,
            header: None,
            include: None,
            mode: None,
        };
        let vars = jmap(&[
            ("B", serde_json::json!("two")),
            ("A", serde_json::json!("one")),
        ]);
        write_setup_output(&so, &vars).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "A=one\nB=two\n");
    }

    #[test]
    fn write_env_header_is_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("o.env");
        let so = crate::config::SetupOutput {
            path: f.to_string_lossy().into_owned(),
            format: crate::config::OutputFormat::Env,
            header: Some("generated by test".into()),
            include: None,
            mode: None,
        };
        write_setup_output(&so, &jmap(&[("K", serde_json::json!("v"))])).unwrap();
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.starts_with("# generated by test\nK=v\n"));
    }

    #[test]
    fn write_env_include_filters_keys() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("o.env");
        let so = crate::config::SetupOutput {
            path: f.to_string_lossy().into_owned(),
            format: crate::config::OutputFormat::Env,
            header: None,
            include: Some(vec!["KEEP".into()]),
            mode: None,
        };
        let vars = jmap(&[
            ("KEEP", serde_json::json!("yes")),
            ("DROP", serde_json::json!("no")),
        ]);
        write_setup_output(&so, &vars).unwrap();
        let body = std::fs::read_to_string(&f).unwrap();
        assert_eq!(body, "KEEP=yes\n");
    }

    #[test]
    fn write_env_quotes_value_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("o.env");
        let so = crate::config::SetupOutput {
            path: f.to_string_lossy().into_owned(),
            format: crate::config::OutputFormat::Env,
            header: None,
            include: None,
            mode: None,
        };
        let vars = jmap(&[
            ("MSG", serde_json::json!("hello world")),
            ("Q", serde_json::json!(r#"a"b\c"#)),
        ]);
        write_setup_output(&so, &vars).unwrap();
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.contains("MSG=\"hello world\"\n"));
        assert!(body.contains(r#"Q="a\"b\\c""#));
    }

    #[test]
    fn write_env_no_tmp_leftover_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("o.env");
        let so = crate::config::SetupOutput {
            path: f.to_string_lossy().into_owned(),
            format: crate::config::OutputFormat::Env,
            header: None,
            include: None,
            mode: None,
        };
        write_setup_output(&so, &jmap(&[("A", serde_json::json!("1"))])).unwrap();
        // overwrite again (atomic) and confirm no .tmp sibling remains
        write_setup_output(&so, &jmap(&[("A", serde_json::json!("2"))])).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "A=2\n");
        let mut tmp = f.into_os_string();
        tmp.push(".tmp");
        assert!(!std::path::Path::new(&tmp).exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_mode_applied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("secret.env");
        atomic_write(&f, b"X=1\n", Some(0o600)).unwrap();
        let m = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o600);
    }

    #[tokio::test]
    async fn write_env_processor_reads_ctx_vars() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.env");
        let mut c = ctx();
        c.set("TOKEN", "abc");
        let s = format!(
            "type=\"write_env\"\npath=\"{}\"\ninclude=[\"TOKEN\"]",
            p(&f)
        );
        WriteEnvProcessor
            .run(&step(&s), &c, &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "TOKEN=abc\n");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn symlink_dir_falls_back_to_junction_or_copy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        std::fs::write(dir.path().join("src/sub/a.txt"), "hi").unwrap();
        let s = format!(
            "type=\"symlink\"\nsrc=\"{}\"\ndest=\"{}\"",
            p(&dir.path().join("src")),
            p(&dir.path().join("link"))
        );
        SymlinkProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("link/sub/a.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn extract_tar_bz2_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.tar.bz2");
        {
            let f = std::fs::File::create(&p).unwrap();
            let enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::default());
            let mut tb = tar::Builder::new(enc);
            let data = b"bz";
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o644);
            tb.append_data(&mut h, "f.txt", &data[..]).unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }
        let out = dir.path().join("o");
        extract_archive(&p, &out, 0).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("f.txt")).unwrap(), "bz");
    }

    #[tokio::test]
    async fn backup_copies_existing_file_timestamped() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("config.toml");
        std::fs::write(&src, "x = 1\n").unwrap();
        let s = format!("type=\"backup\"\npath=\"{}\"", p(&src));
        BackupProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        let baks: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(
            std::fs::read_to_string(baks[0].path()).unwrap(),
            "x = 1\n"
        );
    }

    #[tokio::test]
    async fn backup_missing_path_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let s = format!("type=\"backup\"\npath=\"{}\"", p(&d.path().join("nope")));
        let out = BackupProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert!(out.skipped);
    }

    #[tokio::test]
    async fn backup_dry_run_no_file() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("c");
        std::fs::write(&src, "v").unwrap();
        let mut c = ctx();
        c.set_dry_run(true);
        let s = format!("type=\"backup\"\npath=\"{}\"", p(&src));
        BackupProcessor
            .run(&step(&s), &c, &NullReporter, &EnvResolver)
            .await
            .unwrap();
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1); // only the source
    }

    #[tokio::test]
    async fn backup_custom_dir_and_suffix() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("c.yaml");
        std::fs::write(&src, "k: v\n").unwrap();
        let bdir = d.path().join("backups");
        let s = format!(
            "type=\"backup\"\npath=\"{}\"\ndir=\"{}\"\nsuffix=\"orig\"",
            p(&src),
            p(&bdir)
        );
        BackupProcessor
            .run(&step(&s), &ctx(), &NullReporter, &EnvResolver)
            .await
            .unwrap();
        let f = std::fs::read_dir(&bdir).unwrap().next().unwrap().unwrap();
        assert!(f.file_name().to_string_lossy().ends_with(".orig"));
    }

    #[test]
    fn render_env_array_becomes_csv() {
        let v: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({"INSTALL_TOOLS": ["node", "ts", "go"]}),
        )
        .unwrap();
        assert_eq!(render_env_body(&v, None, None), "INSTALL_TOOLS=node,ts,go\n");
    }

    #[test]
    fn render_env_empty_array_is_bare_key() {
        let v: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({"INSTALL_PLUGINS": []})).unwrap();
        assert_eq!(render_env_body(&v, None, None), "INSTALL_PLUGINS=\n");
    }

    #[test]
    fn render_env_array_skips_non_scalar_elements() {
        let v: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({"K": ["a", {"x": 1}, "b", [9], 3, true]}),
        )
        .unwrap();
        // object + nested array dropped; string/number/bool kept, in order.
        assert_eq!(render_env_body(&v, None, None), "K=a,b,3,true\n");
    }

    #[test]
    fn render_env_csv_quoted_when_element_has_space() {
        let v: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({"K": ["a b", "c"]})).unwrap();
        // a space in any element forces dotenv quoting of the whole CSV;
        // the comma itself never triggers quoting.
        assert_eq!(render_env_body(&v, None, None), "K=\"a b,c\"\n");
    }

    #[test]
    fn render_env_array_respects_include_allowlist() {
        let v: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({"KEEP": [1, 2], "DROP": ["x"], "S": "scalar"}),
        )
        .unwrap();
        let inc = vec!["KEEP".to_string(), "S".to_string()];
        assert_eq!(
            render_env_body(&v, None, Some(&inc)),
            "KEEP=1,2\nS=scalar\n"
        );
    }
}
