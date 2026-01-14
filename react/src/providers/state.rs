use async_trait::async_trait;
use serde_json::Value;

/// Optional state store for long-running / multi-iteration suites.
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get_json(&self, key: &str) -> Result<Option<Value>, String>;
    async fn put_json(&self, key: &str, value: &Value) -> Result<(), String>;
    async fn delete(&self, key: &str) -> Result<(), String>;
}

