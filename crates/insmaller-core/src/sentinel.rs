//! Filesystem install markers. Ported from the reference installer's sentinel.rs. The dir
//! name comes from `[settings].sentinel_dir_name` (no global OnceLock — a
//! `Sentinel` value carries the base, which also makes it test-injectable).

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs::{File, TryLockError};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct SentinelData {
    pub version: Option<String>,
    pub installed_at: String,
    pub spec: String,
}

#[derive(Debug, Clone)]
pub struct Sentinel {
    base: PathBuf,
}

/// RAII handle for [`Sentinel::lock`]; releases the cross-process lock on drop.
pub struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl Sentinel {
    /// Production: `<data_local_dir>/<dir_name>`.
    pub fn new(dir_name: &str) -> Self {
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(dir_name);
        Self { base }
    }

    /// Tests / hosts that want an explicit root.
    pub fn with_base(base: PathBuf) -> Self {
        Self { base }
    }

    /// Scope-aware construction. Precedence (highest first):
    /// `sentinel_path` (explicit, `~`-expanded) → `workspace`
    /// (`<config_dir>/.<sentinel_dir_name>`) → `global` (the historical
    /// `<data_local_dir>/<sentinel_dir_name>`). `workspace` with no
    /// `config_dir` falls back to `global`. Default settings ⇒ identical to
    /// `Sentinel::new(&settings.sentinel_dir_name)`.
    pub fn resolve(
        settings: &crate::config::Settings,
        config_dir: Option<&std::path::Path>,
    ) -> Self {
        use crate::config::SentinelScope;
        if let Some(p) = &settings.sentinel_path {
            let expanded = crate::pathenv::expand_home(p).unwrap_or_else(|_| p.clone());
            return Self::with_base(PathBuf::from(expanded));
        }
        match (settings.sentinel_scope, config_dir) {
            (SentinelScope::Workspace, Some(dir)) => {
                Self::with_base(dir.join(format!(".{}", settings.sentinel_dir_name)))
            }
            _ => Self::new(&settings.sentinel_dir_name),
        }
    }

    /// Defense-in-depth: a catalog `kind`/`key` must not contain path
    /// separators or `..`, so a sentinel write can never escape the base
    /// (even though the catalog is inside the trust boundary).
    fn safe(seg: &str) -> String {
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || seg.contains(['/', '\\'])
            || seg.contains("..")
        {
            // Collapse to an inert, in-base token rather than panic.
            return format!("_invalid_{}", seg.replace(['/', '\\', '.'], "_"));
        }
        seg.to_string()
    }
    fn path(&self, kind: &str, key: &str) -> PathBuf {
        self.base
            .join(Self::safe(kind))
            .join(format!("{}.installed", Self::safe(key)))
    }
    fn post_path(&self, kind: &str, key: &str) -> PathBuf {
        self.base
            .join(Self::safe(kind))
            .join(format!("{}.post", Self::safe(key)))
    }

    /// The resolved base directory (host introspection / `insmaller status`).
    pub fn base(&self) -> &std::path::Path {
        &self.base
    }

    /// Acquire a cross-process exclusive lock for mutating operations
    /// (install/uninstall). If another instance holds it, print a one-time
    /// notice and block until it's free, so concurrent runs serialize instead
    /// of racing the sentinel/double-running recipes. The returned handle holds
    /// the lock until dropped. `None` ⇒ locking unavailable; the caller should
    /// proceed unlocked rather than fail (a missing lock must not block work).
    pub fn lock(&self) -> Option<LockGuard> {
        let _ = std::fs::create_dir_all(&self.base);
        let f = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.base.join(".lock"))
            .ok()?;
        match f.try_lock() {
            Ok(()) => Some(LockGuard(f)),
            Err(TryLockError::WouldBlock) => {
                eprintln!("insmaller: another instance is running; waiting…");
                f.lock().ok()?;
                Some(LockGuard(f))
            }
            Err(TryLockError::Error(_)) => None,
        }
    }

    pub fn is_installed(&self, kind: &str, key: &str) -> bool {
        self.path(kind, key).exists()
    }

    pub fn mark(&self, kind: &str, key: &str, spec: &str, version: Option<String>) -> Result<()> {
        let p = self.path(kind, key);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = SentinelData {
            version,
            installed_at: chrono::Utc::now().to_rfc3339(),
            spec: spec.to_string(),
        };
        std::fs::write(
            &p,
            serde_json::to_string_pretty(&data).expect("SentinelData serializes"),
        )?;
        Ok(())
    }

    pub fn read(&self, kind: &str, key: &str) -> Option<SentinelData> {
        let raw = std::fs::read_to_string(self.path(kind, key)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn remove(&self, kind: &str, key: &str) -> Result<()> {
        let p = self.path(kind, key);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    pub fn post_install_done(&self, kind: &str, key: &str) -> bool {
        self.post_path(kind, key).exists()
    }

    pub fn mark_post_install(&self, kind: &str, key: &str) -> Result<()> {
        let p = self.post_path(kind, key);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, chrono::Utc::now().to_rfc3339())?;
        Ok(())
    }

    pub fn remove_post(&self, kind: &str, key: &str) -> Result<()> {
        let p = self.post_path(kind, key);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    /// Every installed `(kind, key)` across all kind subdirs — used by the
    /// uninstall reverse-dependency guard to find still-installed dependents.
    pub fn installed(&self) -> Vec<(String, String)> {
        if !self.base.exists() {
            return vec![];
        }
        walkdir::WalkDir::new(&self.base)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_dir())
            .flat_map(|d| {
                let kind = d.file_name().to_string_lossy().into_owned();
                self.list_kind(&kind)
                    .into_iter()
                    .map(move |k| (kind.clone(), k))
            })
            .collect()
    }

    pub fn list_kind(&self, kind: &str) -> Vec<String> {
        let dir = self.base.join(kind);
        if !dir.exists() {
            return vec![];
        }
        walkdir::WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "installed").unwrap_or(false))
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> (tempfile::TempDir, Sentinel) {
        let d = tempfile::tempdir().unwrap();
        let s = Sentinel::with_base(d.path().to_path_buf());
        (d, s)
    }

    #[test]
    fn mark_read_remove_roundtrip() {
        let (_d, s) = sent();
        assert!(!s.is_installed("tools", "node"));
        s.mark("tools", "node", "nvm:lts", Some("20".into())).unwrap();
        assert!(s.is_installed("tools", "node"));
        let data = s.read("tools", "node").unwrap();
        assert_eq!(data.version.as_deref(), Some("20"));
        assert_eq!(data.spec, "nvm:lts");
        s.remove("tools", "node").unwrap();
        assert!(!s.is_installed("tools", "node"));
    }

    #[test]
    fn lock_is_acquired_and_blocks_a_second_holder() {
        let (_d, s) = sent();
        let guard = s.lock().expect("first lock should acquire");
        assert!(s.base().join(".lock").exists());
        // A second, independent handle to the same lockfile must not be able to
        // take it while the guard is held.
        let other = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(s.base().join(".lock"))
            .unwrap();
        assert!(
            matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
            "lock should be held exclusively while the guard is alive"
        );
        drop(guard);
        // After release, it can be taken again.
        assert!(other.try_lock().is_ok(), "lock should free on guard drop");
    }

    #[test]
    fn post_gate_is_independent_of_install_marker() {
        let (_d, s) = sent();
        s.mark("cli", "claude", "spec", None).unwrap();
        assert!(!s.post_install_done("cli", "claude"));
        s.mark_post_install("cli", "claude").unwrap();
        assert!(s.post_install_done("cli", "claude"));
        s.remove_post("cli", "claude").unwrap();
        assert!(!s.post_install_done("cli", "claude"));
        // install marker untouched by post operations
        assert!(s.is_installed("cli", "claude"));
    }

    #[test]
    fn list_kind_returns_installed_keys() {
        let (_d, s) = sent();
        s.mark("plugins", "a", "x", None).unwrap();
        s.mark("plugins", "b", "y", None).unwrap();
        s.mark_post_install("plugins", "a").unwrap(); // .post must not be listed
        let mut got = s.list_kind("plugins");
        got.sort();
        assert_eq!(got, vec!["a", "b"]);
    }

    use crate::config::{Settings, SentinelScope};

    #[test]
    fn scope_defaults_global_matches_new() {
        let s = Settings::default();
        assert_eq!(
            Sentinel::resolve(&s, Some(std::path::Path::new("/some/proj"))).base,
            Sentinel::new(&s.sentinel_dir_name).base,
            "default scope must be byte-identical to the historical path"
        );
    }

    #[test]
    fn sentinel_path_overrides_scope() {
        let mut s = Settings::default();
        s.sentinel_scope = SentinelScope::Workspace;
        s.sentinel_path = Some("/explicit/base".into());
        assert_eq!(
            Sentinel::resolve(&s, Some(std::path::Path::new("/proj"))).base,
            PathBuf::from("/explicit/base")
        );
    }

    #[test]
    fn workspace_anchors_to_config_dir() {
        let mut s = Settings::default();
        s.sentinel_scope = SentinelScope::Workspace;
        assert_eq!(
            Sentinel::resolve(&s, Some(std::path::Path::new("/proj"))).base,
            PathBuf::from("/proj").join(".insmaller")
        );
    }

    #[test]
    fn workspace_without_config_dir_falls_back_global() {
        let mut s = Settings::default();
        s.sentinel_scope = SentinelScope::Workspace;
        assert_eq!(
            Sentinel::resolve(&s, None).base,
            Sentinel::new(&s.sentinel_dir_name).base
        );
    }
}
