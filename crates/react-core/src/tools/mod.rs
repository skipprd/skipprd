use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

use crate::agent::AgentCtx;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    async fn call(&self, args: Value, ctx: &AgentCtx) -> Result<Value, String>;
}

pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }
    pub fn register<T: Tool + 'static>(&mut self, t: T) {
        self.tools.insert(t.name(), Box::new(t));
    }
    pub async fn call(&self, name: &str, args: Value, ctx: &AgentCtx) -> Result<Value, String> {
        match self.tools.get(name) {
            Some(t) => t.call(args, ctx).await,
            None => Err(format!("unknown tool '{}'", name)),
        }
    }
}
