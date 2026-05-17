use crate::processor::Processor;
use std::collections::HashMap;
use std::sync::Arc;

/// Maps a step `type` to its processor. Seeded with the built-in set; a host
/// can register additional processors (the future external/plugin arm slots
/// in here without engine changes).
#[derive(Default)]
pub struct ProcessorRegistry {
    map: HashMap<String, Arc<dyn Processor>>,
}

impl ProcessorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, p: Arc<dyn Processor>) -> &mut Self {
        self.map.insert(p.kind().to_string(), p);
        self
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn Processor>> {
        self.map.get(kind).cloned()
    }

    pub fn known(&self) -> Vec<&str> {
        self.map.keys().map(String::as_str).collect()
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

    #[test]
    fn register_and_lookup() {
        let mut r = ProcessorRegistry::new();
        r.register(Arc::new(Noop));
        assert!(r.get("noop").is_some());
        assert!(r.get("missing").is_none());
        assert_eq!(r.known(), vec!["noop"]);
    }
}
