use serde_json::{json, Value};

pub fn parse_allintitle_result(
    result_body: &Value,
    site: &str,
    run_date: &str,
    keyword: &str,
    search_volume: u64,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Value {
    let allintitle_count = result_body
        .get("se_results_count")
        .and_then(json_u64)
        .or_else(|| {
            result_body
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| arr.len() as u64)
        })
        .unwrap_or(0);
    let kgr = if search_volume > 0 {
        allintitle_count as f64 / search_volume as f64
    } else {
        0.0
    };
    let kgr_bucket = kgr_bucket_label(kgr);
    let exact_title_competition_score = if allintitle_count == 0 {
        0.0
    } else if allintitle_count <= 10 {
        0.1
    } else if allintitle_count <= 50 {
        0.35
    } else if allintitle_count <= 250 {
        0.65
    } else {
        0.9
    };
    json!({
        "site": site,
        "run_date": run_date,
        "keyword": keyword,
        "allintitle_count": allintitle_count,
        "search_volume": search_volume,
        "kgr": kgr,
        "kgr_bucket": kgr_bucket,
        "exact_title_competition_score": exact_title_competition_score,
        "query_status": "ok",
        "location_code": location_code,
        "language_code": language_code,
        "device": device,
    })
}

fn kgr_bucket_label(kgr: f64) -> &'static str {
    if kgr <= 0.25 {
        "golden"
    } else if kgr <= 1.0 {
        "good"
    } else if kgr <= 2.0 {
        "moderate"
    } else {
        "hard"
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_live_response;

    #[test]
    fn parse_allintitle_fixture() {
        let bytes = std::fs::read(format!(
            "{}/fixtures/allintitle_serp_live.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let row = parse_allintitle_result(
            parsed.tasks[0].result_body.as_ref().unwrap(),
            "example.com",
            "2026-05-30",
            "meal planning app free",
            2400,
            2840,
            "en",
            "desktop",
        );
        assert_eq!(row["allintitle_count"], 42);
        assert_eq!(row["kgr_bucket"], "golden");
    }
}
