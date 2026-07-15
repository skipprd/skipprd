use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrawlProgressFields {
    pub crawl_run_id: Option<String>,
    pub batch_index: Option<i64>,
    pub url_count: Option<i64>,
    pub batches_completed: Option<i64>,
    pub batches_total: Option<i64>,
}

fn parse_i64_env(name: &str) -> Option<i64> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
}

pub fn crawl_progress_fields() -> CrawlProgressFields {
    CrawlProgressFields {
        crawl_run_id: std::env::var("SKIPPR_CRAWL_RUN_ID")
            .ok()
            .filter(|value| !value.is_empty()),
        batch_index: parse_i64_env("SKIPPR_CRAWL_BATCH_INDEX"),
        url_count: parse_i64_env("SKIPPR_CRAWL_URL_COUNT"),
        batches_completed: parse_i64_env("SKIPPR_CRAWL_BATCHES_COMPLETED"),
        batches_total: parse_i64_env("SKIPPR_CRAWL_BATCHES_TOTAL"),
    }
}

fn insert_optional_string(obj: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        obj.insert(key.to_string(), json!(value));
    }
}

fn insert_optional_i64(obj: &mut Map<String, Value>, key: &str, value: Option<i64>) {
    if let Some(value) = value {
        obj.insert(key.to_string(), json!(value));
    }
}

pub fn merge_crawl_progress(mut row: Value) -> Value {
    let progress = crawl_progress_fields();
    let obj = row
        .as_object_mut()
        .expect("merge_crawl_progress expects a JSON object row");
    insert_optional_string(obj, "crawl_run_id", progress.crawl_run_id);
    insert_optional_i64(obj, "batch_index", progress.batch_index);
    insert_optional_i64(obj, "url_count", progress.url_count);
    insert_optional_i64(obj, "batches_completed", progress.batches_completed);
    insert_optional_i64(obj, "batches_total", progress.batches_total);
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_crawl_progress_env_fields() {
        std::env::set_var("SKIPPR_CRAWL_RUN_ID", "run-abc");
        std::env::set_var("SKIPPR_CRAWL_BATCH_INDEX", "2");
        std::env::set_var("SKIPPR_CRAWL_URL_COUNT", "120");
        std::env::set_var("SKIPPR_CRAWL_BATCHES_COMPLETED", "1");
        std::env::set_var("SKIPPR_CRAWL_BATCHES_TOTAL", "5");

        let row = merge_crawl_progress(json!({ "site": "https://example.com" }));
        assert_eq!(row["crawl_run_id"], "run-abc");
        assert_eq!(row["batch_index"], 2);
        assert_eq!(row["url_count"], 120);
        assert_eq!(row["batches_completed"], 1);
        assert_eq!(row["batches_total"], 5);

        std::env::remove_var("SKIPPR_CRAWL_RUN_ID");
        std::env::remove_var("SKIPPR_CRAWL_BATCH_INDEX");
        std::env::remove_var("SKIPPR_CRAWL_URL_COUNT");
        std::env::remove_var("SKIPPR_CRAWL_BATCHES_COMPLETED");
        std::env::remove_var("SKIPPR_CRAWL_BATCHES_TOTAL");
    }
}
