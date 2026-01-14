use std::collections::HashMap;
use std::sync::Arc;

use super::{DynSuite, Suite};

pub struct SuiteRegistry {
    suites: HashMap<&'static str, DynSuite>,
}

impl SuiteRegistry {
    pub fn new() -> Self {
        Self { suites: HashMap::new() }
    }

    pub fn register<S: Suite + 'static>(&mut self, suite: S) {
        let id = suite.id();
        self.suites.insert(id, Arc::new(suite));
    }

    pub fn get(&self, suite_id: &str) -> Option<DynSuite> {
        self.suites.get(suite_id).cloned()
    }

    pub fn list_ids(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self.suites.keys().copied().collect();
        out.sort();
        out
    }
}

pub fn default_registry() -> SuiteRegistry {
    let mut reg = SuiteRegistry::new();
    reg.register(crate::react::suites::skippr_ask_suite::SkipprAskSuite);
    reg.register(crate::react::suites::skippr_model_suite::SkipprModelSuite);
    reg
}

