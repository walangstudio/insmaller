/// UI-agnostic progress/log sink. Replaces the direct `cliclack::log` leak
/// that coupled the reference installer's orchestrator to a TUI. The container
/// injects a stdout reporter; an interactive host can inject a cliclack one.
pub trait Reporter: Send + Sync {
    fn step_start(&self, key: &str, step_type: &str);
    fn step_end(&self, key: &str, step_type: &str, ok: bool);
    fn log(&self, msg: &str);
}

pub struct StdoutReporter;

impl Reporter for StdoutReporter {
    fn step_start(&self, key: &str, step_type: &str) {
        println!("[{key}] {step_type} …");
    }
    fn step_end(&self, key: &str, step_type: &str, ok: bool) {
        println!("[{key}] {step_type} {}", if ok { "ok" } else { "FAILED" });
    }
    fn log(&self, msg: &str) {
        println!("{msg}");
    }
}

/// Structured reporter: one JSON object per event to stdout. Feeds CI / the
/// the reference installer catalog-smoke status table.
pub struct JsonReporter;

impl JsonReporter {
    fn emit(&self, ev: &str, key: &str, step: &str, ok: Option<bool>, msg: Option<&str>) {
        let mut o = serde_json::Map::new();
        o.insert("event".into(), ev.into());
        if !key.is_empty() {
            o.insert("key".into(), key.into());
        }
        if !step.is_empty() {
            o.insert("step".into(), step.into());
        }
        if let Some(b) = ok {
            o.insert("ok".into(), b.into());
        }
        if let Some(m) = msg {
            o.insert("message".into(), m.into());
        }
        println!("{}", serde_json::Value::Object(o));
    }
}

impl Reporter for JsonReporter {
    fn step_start(&self, key: &str, step_type: &str) {
        self.emit("step_start", key, step_type, None, None);
    }
    fn step_end(&self, key: &str, step_type: &str, ok: bool) {
        self.emit("step_end", key, step_type, Some(ok), None);
    }
    fn log(&self, msg: &str) {
        self.emit("log", "", "", None, Some(msg));
    }
}

/// Discards everything. Used by tests.
pub struct NullReporter;

impl Reporter for NullReporter {
    fn step_start(&self, _: &str, _: &str) {}
    fn step_end(&self, _: &str, _: &str, _: bool) {}
    fn log(&self, _: &str) {}
}
