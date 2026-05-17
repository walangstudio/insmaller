//! E1 external-process plugins. The registry has no enum arm — an external
//! plugin is just a `Processor` (`ExternalProcessor`) registered per declared
//! `kind`. Transport is injectable so the protocol is unit-testable without a
//! subprocess. One JSON protocol; P5 reuses it for WASM/cdylib.
//!
//! Trust model: an external plugin is arbitrary code with the engine's
//! privileges — exactly as trusted as a `shell` recipe. `sandbox=true` only
//! reduces the inherited environment; it is not a security boundary.

use crate::config::PluginDecl;
use crate::ctx::Ctx;
use crate::input::InputResolver;
use crate::processor::{Processor, StepOutput};
use crate::registry::ProcessorRegistry;
use crate::reporter::Reporter;
use crate::step::Step;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub const PROTOCOL: u8 = 1;

#[derive(Debug, Serialize)]
pub struct PluginRequest {
    pub protocol: u8,
    pub kind: String,
    pub params: Value,
    pub ctx: Value,
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct PluginResponse {
    pub protocol: u8,
    pub ok: bool,
    #[serde(default)]
    pub register: Map<String, Value>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub log: Vec<String>,
    #[serde(default)]
    pub message: String,
}

#[async_trait::async_trait]
pub trait PluginTransport: Send + Sync {
    async fn invoke(&self, request_json: &str) -> Result<String>;
}

/// Render a step's JSON params, ctx-rendering every string leaf (external
/// plugins receive final values, never `{{ }}`).
pub fn render_params(step: &Step, ctx: &Ctx) -> Result<Value> {
    fn conv(v: &Value, ctx: &Ctx) -> Result<Value> {
        Ok(match v {
            Value::String(s) => Value::String(ctx.render(s)?),
            Value::Array(a) => {
                Value::Array(a.iter().map(|x| conv(x, ctx)).collect::<Result<_>>()?)
            }
            Value::Object(t) => {
                let mut m = Map::new();
                for (k, x) in t {
                    m.insert(k.clone(), conv(x, ctx)?);
                }
                Value::Object(m)
            }
            other => other.clone(),
        })
    }
    let mut m = Map::new();
    for (k, v) in &step.params {
        m.insert(k.clone(), conv(v, ctx)?);
    }
    Ok(Value::Object(m))
}

pub struct ExternalProcessor {
    kind: String,
    transport: Arc<dyn PluginTransport>,
}

#[async_trait::async_trait]
impl Processor for ExternalProcessor {
    fn kind(&self) -> &str {
        &self.kind
    }
    async fn run(
        &self,
        step: &Step,
        ctx: &Ctx,
        rep: &dyn Reporter,
        _i: &dyn InputResolver,
    ) -> Result<StepOutput> {
        let req = PluginRequest {
            protocol: PROTOCOL,
            kind: self.kind.clone(),
            params: render_params(step, ctx)?,
            ctx: ctx.vars_json(),
            dry_run: ctx.dry_run(),
        };
        let raw = self
            .transport
            .invoke(&serde_json::to_string(&req)?)
            .await
            .with_context(|| format!("plugin '{}' transport", self.kind))?;
        let resp: PluginResponse = serde_json::from_str(&raw)
            .with_context(|| format!("plugin '{}' returned non-JSON: {raw}", self.kind))?;
        if resp.protocol != PROTOCOL {
            bail!(
                "plugin '{}' speaks protocol {} but engine speaks {PROTOCOL}",
                self.kind,
                resp.protocol
            );
        }
        for l in &resp.log {
            rep.log(&format!("[plugin {}] {l}", self.kind));
        }
        if !resp.ok {
            bail!(
                "plugin '{}' failed: {}",
                self.kind,
                if resp.message.is_empty() {
                    "(no message)"
                } else {
                    &resp.message
                }
            );
        }
        Ok(StepOutput {
            register: resp.register,
            value: resp.value,
            skipped: false,
        })
    }
}

/// Minimal quote-aware command splitter: groups `"…"` / `'…'` segments so a
/// program path with spaces (e.g. Windows `C:\Program Files\p.exe`) survives.
/// Not a full shell parser (no escaping/vars) — enough for plugin commands.
pub fn split_command(cmd: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("plugin command has an unterminated quote: {cmd}");
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Subprocess transport: JSON on stdin → JSON on stdout, exit code = status.
pub struct ProcessTransport {
    pub command: String,
    pub sandbox: bool,
    pub pass_env: Vec<String>,
    pub timeout: Option<Duration>,
}

#[async_trait::async_trait]
impl PluginTransport for ProcessTransport {
    async fn invoke(&self, request_json: &str) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        let parts = split_command(&self.command)?;
        let (program, args) = parts
            .split_first()
            .context("plugin command is empty")?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Engine-level timeout drops this future; kill the child too so a
            // hung plugin doesn't outlive the step (gap #2 for the process
            // transport).
            .kill_on_drop(true);
        if self.sandbox {
            cmd.env_clear();
            for k in ["PATH", "HOME", "SYSTEMROOT", "USERPROFILE"] {
                if let Ok(v) = std::env::var(k) {
                    cmd.env(k, v);
                }
            }
            for k in &self.pass_env {
                if let Ok(v) = std::env::var(k) {
                    cmd.env(k, v);
                }
            }
        }

        let mut child = cmd.spawn().with_context(|| format!("spawn '{program}'"))?;
        child
            .stdin
            .take()
            .context("no stdin")?
            .write_all(request_json.as_bytes())
            .await?;
        let fut = child.wait_with_output();
        let out = match self.timeout {
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(r) => r?,
                Err(_) => bail!("plugin '{program}' timed out"),
            },
            None => fut.await?,
        };
        if !out.status.success() {
            bail!(
                "plugin '{program}' exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

// ── P5: WASM + native cdylib transports (feature-gated) ─────────────────────
// Same JSON protocol as the subprocess transport — one protocol, three
// transports. Default build compiles neither (offline/dep-light).

/// Native dynamic library plugin. C ABI:
///   `insmaller_plugin_run(req:*const u8, len:usize, out_len:*mut usize) -> *mut u8`
///   `insmaller_plugin_free(ptr:*mut u8, len:usize)`
/// No sandbox — as trusted as a `shell` recipe.
#[cfg(feature = "cdylib")]
pub struct CdylibTransport {
    pub path: String,
}

#[cfg(feature = "cdylib")]
#[async_trait::async_trait]
impl PluginTransport for CdylibTransport {
    async fn invoke(&self, request_json: &str) -> Result<String> {
        let (path, req) = (self.path.clone(), request_json.to_string());
        tokio::task::spawn_blocking(move || -> Result<String> {
            // SAFETY: trusted plugin, documented C ABI; ptr+len from the
            // plugin's own allocator, freed via its free fn.
            unsafe {
                let lib = libloading::Library::new(&path)
                    .with_context(|| format!("dlopen {path}"))?;
                let run: libloading::Symbol<
                    unsafe extern "C" fn(*const u8, usize, *mut usize) -> *mut u8,
                > = lib.get(b"insmaller_plugin_run")?;
                let free: libloading::Symbol<unsafe extern "C" fn(*mut u8, usize)> =
                    lib.get(b"insmaller_plugin_free")?;
                let mut out_len: usize = 0;
                let ptr = run(req.as_ptr(), req.len(), &mut out_len);
                if ptr.is_null() {
                    bail!("cdylib plugin returned null");
                }
                let s = String::from_utf8_lossy(std::slice::from_raw_parts(ptr, out_len))
                    .into_owned();
                free(ptr, out_len);
                Ok(s)
            }
        })
        .await
        .context("cdylib task panicked")?
    }
}

// WASM transport — documented-deferred (extism absent from this box's offline
// cargo cache; an optional dep cargo can't resolve offline breaks the default
// build). To enable when building online:
//   1. add `extism = { version = "1", optional = true }` + feature
//      `wasm = ["dep:extism"]` to Cargo.toml,
//   2. add behind `#[cfg(feature = "wasm")]`:
//        pub struct WasmTransport { pub path: String }
//        impl PluginTransport for WasmTransport { async fn invoke(&self, req)
//          -> Result<String> {  // spawn_blocking:
//            let m = extism::Manifest::new([extism::Wasm::file(&self.path)]);
//            let mut pl = extism::Plugin::new(&m, [], true)?;
//            Ok(pl.call::<&str,&str>("run", req)?.to_string()) } }
//   3. wire it in `register_external` (the `p.wasm` branch already exists,
//      currently skip-with-notice). Same JSON protocol as ProcessTransport —
//      pure drop-in.

/// Register an `ExternalProcessor` for every `kind` of every `[[plugin]]`
/// declaring a transport (`command` | `cdylib` | `wasm`). Called by the host
/// after `builtins()`. Decls whose transport's feature is disabled are
/// skipped with a notice so a default build never breaks on such config.
pub fn register_external(reg: &mut ProcessorRegistry, plugins: &[PluginDecl]) {
    for p in plugins {
        if p.kinds.is_empty() {
            continue;
        }
        let transport: Option<Arc<dyn PluginTransport>> = if let Some(cmd) = &p.command {
            Some(Arc::new(ProcessTransport {
                command: cmd.clone(),
                sandbox: p.sandbox,
                pass_env: p.pass_env.clone(),
                timeout: None,
            }))
        } else if let Some(_lib) = &p.cdylib {
            #[cfg(feature = "cdylib")]
            {
                Some(Arc::new(CdylibTransport { path: _lib.clone() }))
            }
            #[cfg(not(feature = "cdylib"))]
            {
                eprintln!(
                    "insmaller: plugin '{}' needs the `cdylib` feature — skipped",
                    p.name
                );
                None
            }
        } else if p.wasm.is_some() {
            // WASM is documented-deferred (no `wasm` feature/extism dep on
            // this build — see the WasmTransport note above). Skip, don't
            // break the build.
            eprintln!(
                "insmaller: plugin '{}' uses wasm — WASM transport not built (see plugin.rs)",
                p.name
            );
            None
        } else {
            None
        };
        if let Some(t) = transport {
            for k in &p.kinds {
                reg.register(Arc::new(ExternalProcessor {
                    kind: k.clone(),
                    transport: t.clone(),
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::EnvResolver;
    use crate::reporter::NullReporter;

    struct Mock(String);
    #[async_trait::async_trait]
    impl PluginTransport for Mock {
        async fn invoke(&self, _req: &str) -> Result<String> {
            Ok(self.0.clone())
        }
    }
    fn ext(json: &str) -> ExternalProcessor {
        ExternalProcessor {
            kind: "custom".into(),
            transport: Arc::new(Mock(json.into())),
        }
    }
    fn step() -> Step {
        Step::from_table(
            "type=\"custom\"\nfoo=\"hi-{{ key }}\"\nn=3"
                .parse()
                .unwrap(),
        )
        .unwrap()
    }
    fn ctx() -> Ctx {
        let mut c = Ctx::default();
        c.set("key", "x");
        c
    }
    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn split_command_groups_quoted_paths() {
        assert_eq!(
            split_command(r#""C:\Program Files\p.exe" --flag x"#).unwrap(),
            vec![r"C:\Program Files\p.exe", "--flag", "x"]
        );
        assert_eq!(split_command("prog a b").unwrap(), vec!["prog", "a", "b"]);
        assert_eq!(
            split_command("'one two' three").unwrap(),
            vec!["one two", "three"]
        );
        assert_eq!(
            split_command("  spaced   out  ").unwrap(),
            vec!["spaced", "out"]
        );
        // unterminated quote is an error, not a silent partial token
        assert!(split_command(r#"prog "oops"#).is_err());
    }

    #[test]
    fn request_renders_params_and_ctx() {
        let v = render_params(&step(), &ctx()).unwrap();
        assert_eq!(v["foo"], "hi-x");
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn ok_response_maps_register_and_value() {
        let p = ext(r#"{"protocol":1,"ok":true,"register":{"v":"7"},"value":"7"}"#);
        let out = rt()
            .block_on(p.run(&step(), &ctx(), &NullReporter, &EnvResolver))
            .unwrap();
        assert_eq!(out.register.get("v").unwrap(), "7");
        assert_eq!(out.value.unwrap(), "7");
    }

    #[test]
    fn not_ok_response_is_error_with_message() {
        let p = ext(r#"{"protocol":1,"ok":false,"message":"boom"}"#);
        let e = rt()
            .block_on(p.run(&step(), &ctx(), &NullReporter, &EnvResolver))
            .unwrap_err();
        assert!(format!("{e:#}").contains("boom"));
    }

    #[test]
    fn wrong_protocol_is_refused_loudly() {
        let p = ext(r#"{"protocol":9,"ok":true}"#);
        let e = rt()
            .block_on(p.run(&step(), &ctx(), &NullReporter, &EnvResolver))
            .unwrap_err();
        assert!(format!("{e:#}").contains("protocol"));
    }

    #[test]
    fn non_json_response_is_error() {
        let p = ext("not json");
        assert!(rt()
            .block_on(p.run(&step(), &ctx(), &NullReporter, &EnvResolver))
            .is_err());
    }

    #[test]
    fn wasm_cdylib_decl_skipped_without_feature_not_panic() {
        // Default build (no wasm/cdylib feature): such a decl is skipped, not
        // a hard error — a config mentioning them must not break a base build.
        let mut reg = ProcessorRegistry::new();
        let decls = vec![
            PluginDecl {
                name: "w".into(),
                recipe_pack: None,
                command: None,
                wasm: Some("p.wasm".into()),
                cdylib: None,
                kinds: vec!["w1".into()],
                sandbox: false,
                pass_env: vec![],
            },
            PluginDecl {
                name: "c".into(),
                recipe_pack: None,
                command: None,
                wasm: None,
                cdylib: Some("p.so".into()),
                kinds: vec!["c1".into()],
                sandbox: false,
                pass_env: vec![],
            },
        ];
        register_external(&mut reg, &decls);
        assert!(reg.get("w1").is_none()); // wasm always skipped (deferred)
        #[cfg(not(feature = "cdylib"))]
        assert!(reg.get("c1").is_none());
    }

    #[test]
    fn register_external_adds_processor_per_kind() {
        let mut reg = ProcessorRegistry::new();
        let decls = vec![PluginDecl {
            name: "p".into(),
            recipe_pack: None,
            command: Some("true".into()),
            wasm: None,
            cdylib: None,
            kinds: vec!["a".into(), "b".into()],
            sandbox: true,
            pass_env: vec![],
        }];
        register_external(&mut reg, &decls);
        assert!(reg.get("a").is_some() && reg.get("b").is_some());
    }
}
