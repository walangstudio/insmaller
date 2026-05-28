use crate::processor::Processor;
use std::collections::HashMap;
use std::sync::Arc;

/// Maps a step `type` to its processor. Seeded with the built-in set; a host
/// can register additional processors (the future external/plugin arm slots
/// in here without engine changes). Aliases are stored as forwarding entries
/// in a separate table so an override of a canonical kind also overrides
/// every alias pointing at it — no shadow copy of an `Arc` to drift.
#[derive(Default)]
pub struct ProcessorRegistry {
    map: HashMap<String, Arc<dyn Processor>>,
    aliases: HashMap<String, String>,
}

impl ProcessorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, p: Arc<dyn Processor>) -> &mut Self {
        self.map.insert(p.kind().to_string(), p);
        self
    }

    /// Bind `alias` so a step `type = "<alias>"` resolves to the processor
    /// currently registered under `canonical`. Resolution is by name, not by
    /// stored Arc, so a later `register()` overriding `canonical` (e.g. a
    /// plugin replacing the built-in `prompt`) automatically flows through
    /// to every alias.
    pub fn register_alias(&mut self, alias: &str, canonical: &str) -> &mut Self {
        self.aliases.insert(alias.into(), canonical.into());
        self
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn Processor>> {
        let canonical = self
            .aliases
            .get(kind)
            .map(String::as_str)
            .unwrap_or(kind);
        self.map.get(canonical).cloned()
    }

    pub fn known(&self) -> Vec<&str> {
        self.map
            .keys()
            .map(String::as_str)
            .chain(self.aliases.keys().map(String::as_str))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Ctx;
    use crate::input::InputResolver;
    use crate::processor::StepOutput;
    use crate::reporter::Reporter;
    use crate::step::Step;

    struct Noop;
    #[async_trait::async_trait]
    impl Processor for Noop {
        fn kind(&self) -> &str {
            "noop"
        }
        async fn run(
            &self,
            _: &Step,
            _: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            Ok(StepOutput::ok())
        }
    }

    struct OverrideNoop;
    #[async_trait::async_trait]
    impl Processor for OverrideNoop {
        fn kind(&self) -> &str {
            "noop"
        }
        async fn run(
            &self,
            _: &Step,
            _: &Ctx,
            _: &dyn Reporter,
            _: &dyn InputResolver,
        ) -> anyhow::Result<StepOutput> {
            Ok(StepOutput::ok())
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut r = ProcessorRegistry::new();
        r.register(Arc::new(Noop));
        assert!(r.get("noop").is_some());
        assert!(r.get("missing").is_none());
        let known: Vec<&str> = r.known();
        assert_eq!(known.len(), 1);
        assert!(known.contains(&"noop"));
    }

    #[test]
    fn alias_resolves_to_canonical() {
        let mut r = ProcessorRegistry::new();
        let p: Arc<dyn Processor> = Arc::new(Noop);
        r.register(Arc::clone(&p));
        r.register_alias("input", "noop");
        assert!(Arc::ptr_eq(&r.get("noop").unwrap(), &r.get("input").unwrap()));
        assert!(r.known().contains(&"input"));
    }

    #[test]
    fn override_of_canonical_flows_through_alias() {
        // Bug class the forwarding table prevents: a plugin replaces
        // "noop"; the "input" alias must follow, not stay pinned at the
        // built-in. Compare both lookups against the override's identity.
        let mut r = ProcessorRegistry::new();
        let builtin: Arc<dyn Processor> = Arc::new(Noop);
        r.register(Arc::clone(&builtin));
        r.register_alias("input", "noop");
        let plugin: Arc<dyn Processor> = Arc::new(OverrideNoop);
        r.register(Arc::clone(&plugin));
        assert!(Arc::ptr_eq(&r.get("noop").unwrap(), &plugin));
        assert!(Arc::ptr_eq(&r.get("input").unwrap(), &plugin));
    }
}
